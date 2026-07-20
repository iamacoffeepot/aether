use super::{HttpServerCapability, HttpServerConfig, HttpServerHandle};
use crate::kinds::{HttpServerRequest as RequestKind, RegisterRoute};
use aether_actor::Addressable;
use aether_data::Kind as KindTrait;
use aether_data::KindId;
use aether_substrate::Mail;
use aether_substrate::Subname;
use aether_substrate::actor::native::NativeActor;
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::testing::{TestChassis, fresh_substrate};
use aether_trace::TraceDispatchCapability;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use test_handlers::{
    ApiRouteHandler, ApiV2Handler, BookRouteHandler, DeferRouteHandler, EchoHttpHandler, EchoPeer,
    ExclusiveMacroPoolHandler, ExtractRouteHandler, FixedBodyHttpHandler, FloodHttpHandler, MethodAnyHandler,
    MethodPostHandler, STREAM_CHUNK_COUNT, SharedAlphaHandler, SharedBetaHandler, SharedMacroPoolHandler,
    SilentHttpHandler, SilentPeer, StreamHttpHandler, StreamIdEchoHandler, StreamingUploadHandler, TmpRouteHandler,
    WiredRouteHandler, stream_chunk_body,
};

mod test_handlers {
    //! Minimal native handler actors behind the server in the integration
    //! tests: one that replies `200` echoing the request, one that drops
    //! the request without replying (the `502` safety-net path), two
    //! response-streaming handlers (ADR-0128) — a well-behaved one that
    //! paces chunks against credit, and a flooder that ignores credit —
    //! plus the routed handlers. Most route handlers author their routes
    //! through the typed `#[http::router]` / `#[http::route]` surface
    //! (ADR-0131) — the macro mints each route's kind and injects its
    //! `register_route_self` registration — so the tests exercise what a
    //! component author writes. The conflict-`Err` and idempotent
    //! double-claim handlers stay on the raw registration surface, so a
    //! macro regression cannot mask a registration-semantics one.
    use aether_actor::{Manual, actor};
    use aether_data::Kind;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    use crate::{RequestStream, ResponseStream};

    use crate as http;
    use crate::kinds::{
        HttpHeader, HttpRequestChunk, HttpRequestStreamEnd, HttpRequestStreamOpen, HttpResponseStreamOpen,
        HttpServerRequest, HttpServerResponse, HttpStreamCredit, RegisterRouteSelf, UnregisterRouteSelf,
    };
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
    #[actor(singleton)]
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
    #[actor(singleton)]
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
    #[actor(singleton)]
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
    #[actor(singleton)]
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
        fn on_extra(
            _state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            _request: HttpServerRequest,
        ) -> HttpServerResponse {
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
    #[actor(singleton)]
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
            HttpServerResponse {
                status: 200,
                headers: Vec::new(),
                body: format!("books:checkout:{}", id.0).into_bytes(),
            }
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

    #[actor(singleton)]
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
    /// deferred route's `504` settlement net catches.
    pub struct SilentPeer;
    pub struct SilentPeerState;

    #[actor(singleton)]
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
    /// forwards to [`SilentPeer`] and is answered `504` by the settlement
    /// net when that chain settles without a reply.
    pub struct DeferRouteHandler;
    pub struct DeferRouteHandlerState;

    #[http::router]
    #[actor(singleton)]
    impl NativeActor for DeferRouteHandler {
        type State = DeferRouteHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_route_defer";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<DeferRouteHandlerState, BootError> {
            Ok(DeferRouteHandlerState)
        }

        /// `GET /echo` — forward to the echo peer by type, answer on its
        /// reply. `peer::<R>()` names the peer; `.defer` forwards to it.
        #[http::route(Get, "/echo")]
        fn echo(_state: &mut DeferRouteHandlerState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
            ctx.peer::<EchoPeer>().defer(&EchoAsk { text: "hi".to_string() })
        }

        /// `GET /blackhole` — forward to the silent peer; it settles without a
        /// reply, so the server's own `502` net answers.
        #[http::route(Get, "/blackhole")]
        fn blackhole(_state: &mut DeferRouteHandlerState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
            ctx.peer::<SilentPeer>().defer(&EchoAsk { text: "void".to_string() })
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
            #[actor(singleton)]
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

    /// A routed handler whose `wire` registers each claim `shared: true`
    /// with the given dispatch kind (ADR-0136) — the member-set opt-in
    /// the shared-route tests exercise. Replies `200` with a fixed tag
    /// body.
    macro_rules! shared_routed_handler {
        ($ty:ident, $state:ident, $namespace:literal, $tag:literal, $kind:ty,
         [$(($method:expr, $prefix:literal)),+ $(,)?]) => {
            pub struct $ty;
            pub struct $state;

            #[actor(singleton)]
            impl NativeActor for $ty {
                type State = $state;
                type Config = ();
                const NAMESPACE: &'static str = $namespace;

                fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<$state, BootError> {
                    Ok($state)
                }

                fn wire(_state: &mut $state, ctx: &mut NativeCtx<'_>) {
                    $(ctx.actor::<HttpServerCapability>().send(&RegisterRouteSelf {
                        prefix: $prefix.to_string(),
                        method: $method,
                        kind: <$kind as Kind>::ID,
                        shared: true,
                    });)+
                }

                #[handler::single]
                fn on_request(
                    _state: &mut Self::State,
                    _ctx: &mut NativeCtx<'_>,
                    _request: HttpServerRequest,
                ) -> HttpServerResponse {
                    HttpServerResponse {
                        status: 200,
                        headers: Vec::new(),
                        body: $tag.to_vec(),
                    }
                }
            }
        };
    }

    // The /pool member set (ADR-0136): two shared claimants that both
    // serve, exercised over the wire by
    // `shared_route_spreads_across_members`. The shared/exclusive
    // conflict matrix these once paired with is now covered
    // deterministically in `runtime::unit_tests::route_registration`.
    shared_routed_handler!(
        SharedAlphaHandler,
        SharedAlphaHandlerState,
        "aether.http.test_route_shared_alpha",
        b"alpha",
        HttpServerRequest,
        [(None, "/pool")]
    );
    shared_routed_handler!(
        SharedBetaHandler,
        SharedBetaHandlerState,
        "aether.http.test_route_shared_beta",
        b"beta",
        HttpServerRequest,
        [(None, "/pool")]
    );

    /// A `#[http::router(shared)]` handler (issue 2625) — the typed
    /// author-facing surface a scaled component actually writes, as opposed
    /// to [`shared_routed_handler!`]'s hand-written
    /// `RegisterRouteSelf { shared: true, .. }` send. `#[actor(instanced)]`
    /// so a test can spawn two independently-named live instances of this
    /// exact compiled type under `/macro-pool` — the accurate analog of
    /// loading one wasm component `replicas: 2` times (#2626): both
    /// instances' `wire` runs the identical macro-emitted registration send,
    /// so they carry the same minted `Kind::ID` (derived from the compiled
    /// `NAMESPACE` + method name, not the runtime instance name) and can
    /// actually join one member set — two *different* actor types can't,
    /// since each mints its own kind. `Config` is each instance's reply
    /// tag, so the test can tell which instance served a request.
    pub struct SharedMacroPoolHandler;
    pub struct SharedMacroPoolHandlerState {
        tag: &'static [u8],
    }

    #[http::router(shared)]
    #[actor(instanced)]
    impl NativeActor for SharedMacroPoolHandler {
        type State = SharedMacroPoolHandlerState;
        type Config = &'static [u8];
        const NAMESPACE: &'static str = "aether.http.test_route_shared_macro_pool";

        fn init(tag: &'static [u8], _ctx: &mut NativeInitCtx<'_>) -> Result<SharedMacroPoolHandlerState, BootError> {
            Ok(SharedMacroPoolHandlerState { tag })
        }

        // Hand-written (empty) so `#[http::router]` appends its
        // registration send here rather than synthesizing its own `wire`
        // by copying `on_route`'s first-arg pattern verbatim — a
        // synthesized copy would never touch `state`, so whichever name
        // `on_route` uses would either warn unused (a plain `state`) or
        // trip `clippy::used_underscore_binding` (an `_`-prefixed one
        // `on_route` still reads). An author-written `wire` sidesteps
        // both: its own unused first arg is independently `_`-prefixed
        // and never read, and `on_route`'s `state` is read and plain.
        fn wire(_state: &mut SharedMacroPoolHandlerState, ctx: &mut NativeCtx<'_>) {
            // Body is otherwise empty — `#[http::router]` appends this
            // impl's `RegisterRouteSelf` send here, which is what actually
            // uses `ctx` (a plain, non-underscore name since it must be
            // usable by the injected statement).
        }

        #[http::route(any, "/macro-pool")]
        fn on_route(state: &mut SharedMacroPoolHandlerState, _ctx: http::Ctx<'_, NativeCtx<'_>>) -> HttpServerResponse {
            HttpServerResponse { status: 200, headers: Vec::new(), body: state.tag.to_vec() }
        }
    }

    /// A bare `#[http::router]` instanced handler (issue 2827): two
    /// runtime instances of this one compiled actor type claim
    /// `/macro-excl` with the same macro-minted kind. With today's
    /// default (`shared: false`), only one instance can own the route;
    /// if bare routers accidentally default to `shared: true`, both
    /// instances can join and both configured tags become observable.
    pub struct ExclusiveMacroPoolHandler;
    pub struct ExclusiveMacroPoolHandlerState {
        tag: &'static [u8],
    }

    #[http::router]
    #[actor(instanced)]
    impl NativeActor for ExclusiveMacroPoolHandler {
        type State = ExclusiveMacroPoolHandlerState;
        type Config = &'static [u8];
        const NAMESPACE: &'static str = "aether.http.test_route_excl_macro_pool";

        fn init(tag: &'static [u8], _ctx: &mut NativeInitCtx<'_>) -> Result<ExclusiveMacroPoolHandlerState, BootError> {
            Ok(ExclusiveMacroPoolHandlerState { tag })
        }

        fn wire(_state: &mut ExclusiveMacroPoolHandlerState, ctx: &mut NativeCtx<'_>) {
            // Body is otherwise empty; `#[http::router]` appends the
            // registration send that uses `ctx`.
        }

        #[http::route(any, "/macro-excl")]
        fn on_excl(
            state: &mut ExclusiveMacroPoolHandlerState,
            _ctx: http::Ctx<'_, NativeCtx<'_>>,
        ) -> HttpServerResponse {
            HttpServerResponse { status: 200, headers: Vec::new(), body: state.tag.to_vec() }
        }
    }

    /// Bind the calling handler as the `/` catch-all (ADR-0130) — the
    /// shared replacement for the retired `handler_mailbox` default, so a
    /// route-unmatched request reaches that handler.
    fn bind_catch_all(ctx: &mut NativeCtx<'_>) {
        ctx.actor::<HttpServerCapability>().send(&RegisterRouteSelf {
            prefix: "/".to_string(),
            method: None,
            kind: <HttpServerRequest as Kind>::ID,
            shared: false,
        });
    }

    /// Replies `200` and echoes the request's method / path / query /
    /// peer address (as headers) and body (verbatim), so a test can
    /// assert the full request round-tripped to the handler.
    pub struct EchoHttpHandler;

    /// Empty runtime state for the stateless echo handler (ADR-0122: a
    /// stateless cap still names a state type rather than `()` / `Self`).
    pub struct EchoHttpHandlerState;

    #[actor(singleton)]
    impl NativeActor for EchoHttpHandler {
        type State = EchoHttpHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_echo_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<EchoHttpHandlerState, BootError> {
            Ok(EchoHttpHandlerState)
        }

        fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
            bind_catch_all(ctx);
        }

        #[handler::single]
        fn on_request(
            _state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            request: HttpServerRequest,
        ) -> HttpServerResponse {
            let headers = vec![
                HttpHeader { name: "x-aether-method".to_string(), value: format!("{:?}", request.method) },
                HttpHeader { name: "x-aether-path".to_string(), value: request.path.clone() },
                HttpHeader { name: "x-aether-query".to_string(), value: request.query.clone() },
                HttpHeader { name: "x-aether-peer-addr".to_string(), value: request.peer_addr.clone() },
                HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() },
            ];
            HttpServerResponse { status: 200, headers, body: request.body }
        }
    }

    /// Always replies `200` with a fixed non-empty body, regardless of
    /// method — unlike [`EchoHttpHandler`] (which echoes the request
    /// body, empty for HEAD by definition and so unable to prove body
    /// suppression), this handler always has a body to suppress.
    pub struct FixedBodyHttpHandler;

    /// Empty runtime state for the stateless fixed-body handler (ADR-0122).
    pub struct FixedBodyHttpHandlerState;

    #[actor(singleton)]
    impl NativeActor for FixedBodyHttpHandler {
        type State = FixedBodyHttpHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_fixed_body_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<FixedBodyHttpHandlerState, BootError> {
            Ok(FixedBodyHttpHandlerState)
        }

        fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
            bind_catch_all(ctx);
        }

        #[handler::single]
        fn on_request(
            _state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            _request: HttpServerRequest,
        ) -> HttpServerResponse {
            HttpServerResponse {
                status: 200,
                headers: vec![HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() }],
                body: b"fixed body".to_vec(),
            }
        }
    }

    /// Receives the request and returns without replying — the response-less
    /// chain the `502` settlement safety net covers.
    pub struct SilentHttpHandler;

    /// Empty runtime state for the stateless silent handler (ADR-0122).
    pub struct SilentHttpHandlerState;

    #[actor(singleton)]
    impl NativeActor for SilentHttpHandler {
        type State = SilentHttpHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_silent_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<SilentHttpHandlerState, BootError> {
            Ok(SilentHttpHandlerState)
        }

        fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
            bind_catch_all(ctx);
        }

        #[handler::single]
        fn on_request(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _request: HttpServerRequest) {
            // Intentionally drops the request without replying.
        }
    }

    /// The number of body chunks [`StreamHttpHandler`] emits. Chosen well
    /// above the test's credit window so the round trip exercises credit
    /// replenishment across many refills, not just the initial grant.
    pub const STREAM_CHUNK_COUNT: u32 = 40;

    /// The bytes of chunk `index`: its zero-padded index, so the reassembled
    /// body is the deterministic concatenation `"000001…039"` a test can
    /// rebuild and compare against.
    pub fn stream_chunk_body(index: u32) -> Vec<u8> {
        format!("{index:03}").into_bytes()
    }

    /// A well-behaved response-streaming handler (ADR-0128): replies
    /// `HttpResponseStreamOpen`, then emits [`STREAM_CHUNK_COUNT`] chunks
    /// paced strictly against the credit it is granted, and terminates with
    /// `HttpResponseStreamEnd`.
    pub struct StreamHttpHandler;

    /// Per-stream progress for [`StreamHttpHandler`].
    pub struct StreamHttpHandlerState {
        next_index: u32,
        ended: bool,
    }

    #[actor(singleton)]
    impl NativeActor for StreamHttpHandler {
        type State = StreamHttpHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_stream_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<StreamHttpHandlerState, BootError> {
            Ok(StreamHttpHandlerState { next_index: 0, ended: false })
        }

        /// The cap reads this handler's accept-set off the catch-all
        /// binding to take the streaming path.
        fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
            bind_catch_all(ctx);
        }

        #[handler::single]
        fn on_request(
            state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            _request: HttpServerRequest,
        ) -> HttpResponseStreamOpen {
            state.next_index = 0;
            state.ended = false;
            HttpResponseStreamOpen {
                status: 200,
                headers: vec![HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() }],
            }
        }

        /// Spend the granted credit: send up to `credit.credit` more chunks,
        /// then terminate once all [`STREAM_CHUNK_COUNT`] have gone out.
        /// Addressed through the ADR-0133 [`ResponseStream`] handle — the
        /// data phase goes to whichever dispatch shard granted the credit,
        /// never to the supervisor by type (ADR-0135).
        #[handler::manual]
        fn on_credit(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, credit: HttpStreamCredit) {
            let Some(stream) = ResponseStream::from_credit(ctx, &credit) else {
                return;
            };
            let mut budget = credit.credit;
            while budget > 0 && state.next_index < STREAM_CHUNK_COUNT {
                stream.chunk(ctx, stream_chunk_body(state.next_index));
                state.next_index += 1;
                budget -= 1;
            }
            if state.next_index >= STREAM_CHUNK_COUNT && !state.ended {
                stream.end(ctx);
                state.ended = true;
            }
        }
    }

    /// A response-streaming handler whose entire body is the `stream_id` the
    /// cap minted for that stream and handed over in the first
    /// `HttpStreamCredit`. It exists so a test can read the id off the wire
    /// without reaching into handler state.
    pub struct StreamIdEchoHandler;

    /// Guards [`StreamIdEchoHandler`] against re-emitting on replenishment
    /// credit; reset per request, so each request emits exactly one body.
    pub struct StreamIdEchoHandlerState {
        emitted: bool,
    }

    #[actor(singleton)]
    impl NativeActor for StreamIdEchoHandler {
        type State = StreamIdEchoHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_stream_id_echo_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<StreamIdEchoHandlerState, BootError> {
            Ok(StreamIdEchoHandlerState { emitted: false })
        }

        fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
            bind_catch_all(ctx);
        }

        #[handler::single]
        fn on_request(
            state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            _request: HttpServerRequest,
        ) -> HttpResponseStreamOpen {
            state.emitted = false;
            HttpResponseStreamOpen {
                status: 200,
                headers: vec![HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() }],
            }
        }

        #[handler::manual]
        fn on_credit(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, credit: HttpStreamCredit) {
            let Some(stream) = ResponseStream::from_credit(ctx, &credit) else {
                return;
            };
            if state.emitted {
                return;
            }
            stream.chunk(ctx, credit.stream_id.to_string().into_bytes());
            stream.end(ctx);
            state.emitted = true;
        }
    }

    /// The number of chunks [`FloodHttpHandler`] blasts on its first credit,
    /// far more than any small test window — enough that the cap's credit
    /// accounting hits zero and the over-window guard tears the stream down.
    pub const FLOOD_CHUNK_COUNT: u32 = 200;

    /// A misbehaving response-streaming handler (ADR-0128 trust boundary):
    /// it replies `HttpResponseStreamOpen`, then on its first credit ignores
    /// the granted amount entirely and floods [`FLOOD_CHUNK_COUNT`] chunks.
    pub struct FloodHttpHandler;

    /// Guards [`FloodHttpHandler`] against re-flooding on replenishment
    /// credit (which never arrives once the cap tears the stream down).
    pub struct FloodHttpHandlerState {
        flooded: bool,
    }

    #[actor(singleton)]
    impl NativeActor for FloodHttpHandler {
        type State = FloodHttpHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_flood_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<FloodHttpHandlerState, BootError> {
            Ok(FloodHttpHandlerState { flooded: false })
        }

        fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
            bind_catch_all(ctx);
        }

        #[handler::single]
        fn on_request(
            _state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            _request: HttpServerRequest,
        ) -> HttpResponseStreamOpen {
            HttpResponseStreamOpen { status: 200, headers: Vec::new() }
        }

        #[handler::manual]
        fn on_credit(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, credit: HttpStreamCredit) {
            if state.flooded {
                return;
            }
            state.flooded = true;
            let Some(stream) = ResponseStream::from_credit(ctx, &credit) else {
                return;
            };
            for _ in 0..FLOOD_CHUNK_COUNT {
                stream.chunk(ctx, vec![b'x'; 8]);
            }
        }
    }

    /// A streaming *upload* handler (ADR-0128), the request-side mirror of
    /// [`StreamHttpHandler`]: it declares the request-stream vocabulary
    /// (`HttpRequestStreamOpen` in its accept-set is the structural opt-in the
    /// cap reads), grants one credit per [`HttpRequestChunk`] it drains,
    /// accumulates the received byte count, and replies `200` echoing that
    /// count when the stream ends — the reply riding the
    /// [`HttpRequestStreamEnd`] correlation.
    pub struct StreamingUploadHandler;

    /// Per-upload progress for [`StreamingUploadHandler`].
    pub struct StreamingUploadHandlerState {
        received: usize,
        /// The ADR-0133 inbound-stream handle captured at
        /// `HttpRequestStreamOpen` — credit grants go to whichever dispatch
        /// shard opened the stream (ADR-0135), never to the supervisor by
        /// type.
        stream: Option<RequestStream>,
    }

    #[actor(singleton)]
    impl NativeActor for StreamingUploadHandler {
        type State = StreamingUploadHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_upload_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<StreamingUploadHandlerState, BootError> {
            Ok(StreamingUploadHandlerState { received: 0, stream: None })
        }

        /// The cap reads this handler's accept-set off the catch-all
        /// binding to take the request-streaming path.
        fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
            bind_catch_all(ctx);
        }

        #[handler::manual]
        fn on_stream_open(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, open: HttpRequestStreamOpen) {
            state.received = 0;
            state.stream = RequestStream::from_open(ctx, &open);
        }

        /// Count the piece and grant one credit back so the cap delivers the
        /// next — the inbound mirror of [`StreamHttpHandler::on_credit`].
        #[handler::single]
        fn on_chunk(state: &mut Self::State, ctx: &mut NativeCtx<'_>, chunk: HttpRequestChunk) {
            state.received += chunk.body.len();
            if let Some(stream) = &state.stream {
                stream.credit(ctx, 1);
            }
        }

        #[handler::single]
        fn on_stream_end(
            state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            _end: HttpRequestStreamEnd,
        ) -> HttpServerResponse {
            HttpServerResponse {
                status: 200,
                headers: Vec::new(),
                body: format!("received:{}", state.received).into_bytes(),
            }
        }
    }
}

fn config_for(max_request_bytes: usize) -> HttpServerConfig {
    HttpServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        max_request_bytes,
        request_timeout_millis: 5_000,
        ..HttpServerConfig::default()
    }
}

/// Boot a passive chassis holding one handler actor `H` plus the HTTP
/// server cap under `config` — the single-handler shape most server
/// tests share. Multi-handler and cap-only boots stay explicit at their
/// call sites.
fn boot_chassis<H>(config: HttpServerConfig) -> PassiveChassis<TestChassis>
where
    H: NativeActor<Config = ()>,
{
    let (registry, mailer) = fresh_substrate();
    Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<H>(())
        .with_actor::<HttpServerCapability>(config)
        .build_passive()
        .expect("caps boot")
}

fn boot_single_shard_fixed_body() -> PassiveChassis<TestChassis> {
    boot_chassis::<FixedBodyHttpHandler>(HttpServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        dispatch_shards: 1,
        ..HttpServerConfig::default()
    })
}

/// [`boot_chassis`] with `H` (its `wire` binds the `/` catch-all) under a
/// buffered [`config_for`] config (`max_request_bytes`).
fn boot_buffered<H>(max_request_bytes: usize) -> PassiveChassis<TestChassis>
where
    H: NativeActor<Config = ()>,
{
    boot_chassis::<H>(config_for(max_request_bytes))
}

/// [`boot_chassis`] with `H` under a response-streaming [`stream_config_for`]
/// config (credit `window`).
fn boot_response_stream<H>(window: u32) -> PassiveChassis<TestChassis>
where
    H: NativeActor<Config = ()>,
{
    boot_chassis::<H>(stream_config_for(window))
}

/// [`boot_chassis`] with `H` under a request-streaming
/// [`request_stream_config_for`] config (credit `window`).
fn boot_request_stream<H>(window: u32) -> PassiveChassis<TestChassis>
where
    H: NativeActor<Config = ()>,
{
    boot_chassis::<H>(request_stream_config_for(window))
}

/// Server config for the streaming tests (ADR-0128): a small credit window
/// so a multi-chunk response must replenish credit repeatedly, and a flood
/// overruns it fast.
fn stream_config_for(window: u32) -> HttpServerConfig {
    HttpServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        response_stream_window: window,
        ..HttpServerConfig::default()
    }
}

/// Index of the first `\r\n` at or after `from`, or `None`.
fn find_crlf(bytes: &[u8], from: usize) -> Option<usize> {
    (from..bytes.len().saturating_sub(1)).find(|&i| bytes[i] == b'\r' && bytes[i + 1] == b'\n')
}

/// Reassemble a chunked transfer-encoding body (everything after the head's
/// blank line) into its payload, stopping at the zero-length terminator.
fn dechunk(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut pos = 0;
    let mut out: Vec<u8> = Vec::new();
    while pos < bytes.len() {
        let Some(crlf) = find_crlf(bytes, pos) else {
            break;
        };
        let size = usize::from_str_radix(body[pos..crlf].trim(), 16).unwrap_or(0);
        pos = crlf + 2;
        if size == 0 {
            break;
        }
        if pos + size > bytes.len() {
            break;
        }
        out.extend_from_slice(&bytes[pos..pos + size]);
        // Advance past the chunk body and its trailing CRLF.
        pos += size + 2;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn port_of(chassis: &PassiveChassis<TestChassis>) -> u16 {
    chassis.handle::<HttpServerHandle>().expect("HttpServerHandle published").local_port
}

/// Insert `Connection: close` as the last header of a complete request's
/// head. Keep-alive is the HTTP/1.1 default, so a single-shot round-trip
/// that reads to EOF must opt the connection into close; injecting it here
/// keeps every single-shot test's request literal focused on what it
/// exercises. The keep-alive / HTTP-1.0 / idle-timeout tests drive their own
/// sockets and do not go through this helper.
fn with_connection_close(request: &[u8]) -> Vec<u8> {
    let terminator = b"\r\n\r\n";
    let Some(pos) = request.windows(terminator.len()).position(|window| window == terminator) else {
        return request.to_vec();
    };
    let mut out = Vec::with_capacity(request.len() + 19);
    out.extend_from_slice(&request[..pos]);
    out.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    out.extend_from_slice(&request[pos + terminator.len()..]);
    out
}

/// Open a client `TcpStream` to the server's OS-picked port, write the raw
/// request (with `Connection: close` appended, see [`with_connection_close`]),
/// and read the full response (the cap closes after the single response, so
/// the read terminates at EOF).
fn round_trip(port: u16, request: &[u8]) -> String {
    let request = with_connection_close(request);
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    stream.write_all(&request).expect("write request");
    stream.flush().expect("flush request");

    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&response).into_owned()
}

/// The light non-contention test: the cap binds and publishes the bound
/// port.
#[test]
fn binds_and_publishes_port() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<HttpServerCapability>(config_for(1024))
        .build_passive()
        .expect("http server boots");
    assert!(port_of(&chassis) > 0, "bound to an OS-picked port");
}

fn body_of(response: &str) -> &str {
    response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

/// A GET round-trips to the handler and its reply returns as
/// well-formed HTTP/1.1, carrying the parsed path / query / method.
#[test]
fn get_round_trips_to_handler() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(port_of(&chassis), b"GET /hello?name=ada HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "expected 200 status line, got: {response:?}");
    assert!(response.contains("x-aether-method: Get\r\n"), "{response:?}");
    assert!(response.contains("x-aether-path: /hello\r\n"), "{response:?}");
    assert!(response.contains("x-aether-query: name=ada\r\n"), "{response:?}");
    let peer_addr_header = response
        .lines()
        .find_map(|line| line.strip_prefix("x-aether-peer-addr: "))
        .map(|value| value.trim_end_matches('\r'))
        .expect("x-aether-peer-addr header present");
    assert!(
        peer_addr_header.starts_with("127.0.0.1:"),
        "expected the loopback client's address, got: {peer_addr_header:?}",
    );
    assert!(response.contains("Content-Length: 0\r\n"), "{response:?}");
    assert!(response.contains("Date: "), "{response:?}");
    assert!(response.contains("Connection: close\r\n"), "{response:?}");
}

/// A POST round-trips the body verbatim to the handler.
#[test]
fn post_round_trips_body() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(
        port_of(&chassis),
        b"POST /submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "expected 200, got: {response:?}");
    assert!(response.contains("x-aether-method: Post\r\n"), "{response:?}");
    assert_eq!(body_of(&response), "hello", "body echoed verbatim");
}

/// An announced `Content-Length` past the body cap is answered
/// `413` before any dispatch.
#[test]
fn oversize_body_is_413() {
    let chassis = boot_buffered::<EchoHttpHandler>(8);

    let response =
        round_trip(port_of(&chassis), b"POST /big HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 413 "), "expected 413, got: {response:?}");
}

/// A websocket-upgrade request that also declares an oversized
/// `Content-Length` is answered `413` before any dispatch or buffered-body
/// allocation. A valid upgrade handshake always buffers (never streams),
/// so the body-size cap must apply to it the same as any other buffered
/// request rather than being skipped because `ws_key.is_some()`.
#[test]
fn oversize_body_on_ws_upgrade_is_413() {
    let chassis = boot_buffered::<EchoHttpHandler>(8);

    let response = round_trip(
        port_of(&chassis),
        b"GET /ws HTTP/1.1\r\n\
          Host: localhost\r\n\
          Upgrade: websocket\r\n\
          Connection: Upgrade\r\n\
          Sec-WebSocket-Version: 13\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
          Content-Length: 100\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 413 "), "expected 413, got: {response:?}");
}

/// A websocket-upgrade request that also carries the request-smuggling
/// framing shape (both `Content-Length` and `Transfer-Encoding`) is
/// answered `411` before any dispatch — the same framing reject the
/// non-upgrade path applies, not skipped because `ws_key.is_some()`.
///
/// Tripwire: on `origin/main` this returns non-411 because the framing
/// reject sits inside `if ws_key.is_none()` and never runs for a valid
/// upgrade handshake.
#[test]
fn smuggling_on_ws_upgrade_is_411() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    let response = round_trip(
        port_of(&chassis),
        b"GET /ws HTTP/1.1\r\n\
          Host: localhost\r\n\
          Upgrade: websocket\r\n\
          Connection: Upgrade\r\n\
          Sec-WebSocket-Version: 13\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
          Content-Length: 5\r\n\
          Transfer-Encoding: chunked\r\n\r\nhello",
    );
    assert!(response.starts_with("HTTP/1.1 411 "), "expected 411, got: {response:?}");
}

/// A non-enumerated method is answered `501` before any dispatch.
#[test]
fn unknown_method_is_501() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    let response = round_trip(port_of(&chassis), b"FROB /x HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 501 "), "expected 501, got: {response:?}");
}

/// A request matching no route — no handler registered a catch-all — is
/// answered `503` (ADR-0130).
#[test]
fn no_handler_is_503() {
    let (registry, mailer) = fresh_substrate();
    // No handler actor is booted, so nothing registers a `/` catch-all —
    // every request matches no route.
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<HttpServerCapability>(config_for(1024))
        .build_passive()
        .expect("server boots");

    let response = round_trip(port_of(&chassis), b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 503 "), "expected 503, got: {response:?}");
}

/// A handler that receives the request but never replies settles
/// into `502` via the settlement safety net.
#[test]
fn response_less_chain_is_502() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        // TraceDispatchCapability folds trace events into per-root
        // counters and fires settlement once a root drains; without it
        // the server's settlement subscription never wakes.
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<SilentHttpHandler>(())
        .with_actor::<HttpServerCapability>(config_for(1024))
        .build_passive()
        .expect("caps boot");

    // The silent handler binds `/` via async `wire` mail; poll past the
    // pre-registration `503` to the dispatched `502` the settlement net raises.
    let response = round_trip_live(port_of(&chassis), b"GET /drop HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 502 "), "expected 502, got: {response:?}");
}

/// A percent-encoded path is decoded before it reaches the handler
/// (ADR-0108 §2's "the decoded path component"); the query string stays
/// raw.
#[test]
fn percent_encoded_path_is_decoded() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(port_of(&chassis), b"GET /hello%20world?x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "expected 200, got: {response:?}");
    assert!(response.contains("x-aether-path: /hello world\r\n"), "{response:?}");
    assert!(response.contains("x-aether-query: x=1\r\n"), "{response:?}");
}

/// A `Transfer-Encoding: chunked` request to a *buffered* handler is rejected
/// `411`: an unknown-length body has nothing to buffer under (ADR-0128 relaxes
/// this only for a streaming handler, whose accept-set opts it into the
/// incremental path — see `chunked_upload_streams_to_streaming_handler`).
#[test]
fn transfer_encoding_is_411() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    let response = round_trip(
        port_of(&chassis),
        b"POST /chunked HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 411 "), "expected 411, got: {response:?}");
}

/// A request carrying `Expect: 100-continue` receives the interim `100
/// Continue` before the final response.
#[test]
fn expect_continue_gets_100_continue() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the interim-status assertion
    // (a `100 Continue` prefix precedes the dispatched status, so it cannot
    // itself be distinguished from a pre-registration `503` by prefix).
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let response = round_trip(
        port,
        b"POST /submit HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(response.starts_with("HTTP/1.1 100 Continue\r\n\r\n"), "expected interim 100 Continue, got: {response:?}");
    assert!(response.contains("HTTP/1.1 200 OK\r\n"), "expected final 200 after the interim, got: {response:?}");
}

/// A HEAD response carries the handler's headers — including the
/// `Content-Length` the body would have had — but no body bytes; a GET
/// to the same handler still returns the body.
#[test]
fn head_response_suppresses_body() {
    let chassis = boot_buffered::<FixedBodyHttpHandler>(1024);
    let port = port_of(&chassis);

    // First request against the async-registered `/` catch-all: poll it live.
    let head_response = round_trip_live(port, b"HEAD /x HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(head_response.starts_with("HTTP/1.1 200 OK\r\n"), "expected 200, got: {head_response:?}");
    assert!(head_response.contains("Content-Length: 10\r\n"), "{head_response:?}");
    assert_eq!(body_of(&head_response), "", "HEAD must not carry a message body");

    let get_response = round_trip(port, b"GET /x HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(body_of(&get_response), "fixed body");
}

/// A peer accepted past `max_connections` is refused a canned `503`
/// and closed before a reader thread is spawned; it never reaches the
/// handler. `dispatch_shards` is pinned above one so the ceiling is
/// provably global across shards (ADR-0135) — the two held connections
/// land on different shards round-robin, and the supervisor still
/// refuses the third against the shared live count.
///
/// Tripwire: without the assignment-time capacity guard in
/// `HttpSupervisorState::assign_peer`, this connection is accepted and
/// dispatched (or hangs waiting on the handler) instead of being
/// refused; with a per-shard rather than global count, it is accepted
/// because no single shard is at the ceiling.
#[test]
fn over_capacity_connection_is_503() {
    let max_connections = 2;
    let chassis = boot_chassis::<EchoHttpHandler>(HttpServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        max_connections,
        dispatch_shards: 2,
        ..HttpServerConfig::default()
    });

    let port = port_of(&chassis);

    // Fill the connection table: each socket sends a partial request
    // head (no terminating blank line), so its reader thread blocks
    // waiting for more bytes and its `ConnState` stays resident.
    let mut held = Vec::new();
    for _ in 0..max_connections {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
        stream.write_all(b"GET / HTTP/1.1\r\n").expect("write partial request head");
        stream.flush().expect("flush partial request head");
        held.push(stream);
    }

    // Give the dispatcher a moment to drain the `PeerAccepted` events
    // into `connections` before the next connect.
    thread::sleep(Duration::from_millis(200));

    let response = round_trip(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 503 "), "expected 503, got: {response:?}");

    drop(held);
}

/// Concurrent connections under a pinned multi-shard config (ADR-0135) are
/// all served, including keep-alive reuse: four connections land on two
/// shards round-robin, each serves two sequential requests, every reply
/// routes back through the owning shard. Pinned to `dispatch_shards: 2`
/// rather than the auto worker-count sizing so the multi-shard path is
/// exercised even on a low-core CI runner where auto sizing collapses to
/// one shard.
///
/// Tripwire: a shard-assignment bug (posting to a never-spawned shard, a
/// round-robin index error, a reply intercepted by the wrong actor)
/// surfaces here as a hung read or a `502`/`503` on some subset of the
/// connections.
#[test]
fn connections_distribute_across_shards() {
    let chassis = boot_chassis::<EchoHttpHandler>(HttpServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        dispatch_shards: 2,
        ..HttpServerConfig::default()
    });

    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before driving the concurrent
    // connections so none of them races the registration.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut streams: Vec<(TcpStream, Vec<u8>)> = (0..4)
        .map(|_| {
            let stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
            (stream, Vec::new())
        })
        .collect();

    for round in 0..2 {
        // Write this round's request on every connection first, then read
        // every response — so all four connections are in flight across
        // both shards at once, not serialized one connection at a time.
        for (index, (stream, _)) in streams.iter_mut().enumerate() {
            let request = format!("GET /conn{index}/round{round} HTTP/1.1\r\nHost: localhost\r\n\r\n");
            stream.write_all(request.as_bytes()).expect("write request");
            stream.flush().expect("flush request");
        }
        for (index, (stream, carry)) in streams.iter_mut().enumerate() {
            let response = read_one_response(stream, carry);
            assert!(
                response.starts_with("HTTP/1.1 200 "),
                "conn {index} round {round}: expected 200, got: {response:?}",
            );
            assert!(
                response.contains(&format!("x-aether-path: /conn{index}/round{round}")),
                "conn {index} round {round}: reply correlated to the wrong request: {response:?}",
            );
        }
    }
}

/// A streaming handler (ADR-0128) emits its body across more chunks than the
/// credit window, and the cap streams them as chunked transfer-encoding that
/// the client reassembles intact — exercising credit replenishment across
/// many refills, not just the initial grant.
#[test]
fn streamed_response_reassembles_across_credit_window() {
    // Window well below the chunk count so credit must replenish.
    let chassis = boot_response_stream::<StreamHttpHandler>(8);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(port_of(&chassis), b"GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert!(response.contains("Transfer-Encoding: chunked\r\n"), "streamed response is chunked: {response:?}");
    assert!(!response.contains("Content-Length:"), "streamed response omits Content-Length: {response:?}");

    let expected: Vec<u8> = (0..STREAM_CHUNK_COUNT).flat_map(stream_chunk_body).collect();
    assert_eq!(
        dechunk(body_of(&response)).into_bytes(),
        expected,
        "reassembled body matches every emitted chunk in order",
    );
}

/// Tripwire: a handler that floods chunks past its granted credit
/// (ADR-0128 §Consequences trust boundary) is torn down by the cap — the
/// response head and some chunks arrive, but the stream never reaches its
/// terminating zero-length chunk, so a misbehaving producer cannot outrun
/// the window unbounded.
#[test]
fn over_window_flood_tears_the_stream_down() {
    // Tiny window so the flood overruns credit within a few chunks.
    let chassis = boot_response_stream::<FloodHttpHandler>(2);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(port_of(&chassis), b"GET /flood HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert!(
        response.contains("Transfer-Encoding: chunked\r\n"),
        "flood stream head is chunked before teardown: {response:?}",
    );
    assert!(!response.contains("0\r\n\r\n"), "flood stream is torn down before the terminator: {response:?}");
}

/// Server config for the request-streaming tests (ADR-0128): a small inbound
/// credit window so a multi-chunk upload replenishes credit repeatedly. The
/// `max_request_bytes` cap stays the 1 `MiB` default, which the large-upload
/// test deliberately exceeds — streaming bypasses the buffered body cap.
fn request_stream_config_for(window: u32) -> HttpServerConfig {
    HttpServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        request_stream_window: window,
        ..HttpServerConfig::default()
    }
}

/// A multi-megabyte `Content-Length` upload — past the 1 `MiB` buffered
/// `max_request_bytes` cap — streams incrementally to a streaming handler and
/// the echoed byte count matches, so the body never resides whole in the reader
/// or the handler and the buffered cap does not `413` it (ADR-0128). The small
/// window forces credit to replenish across hundreds of chunks.
#[test]
fn large_upload_streams_past_the_buffered_cap() {
    let chassis = boot_request_stream::<StreamingUploadHandler>(4);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live with a cheap zero-length chunked
    // upload before the multi-megabyte assertion, so the large body is not
    // re-sent on each poll iteration.
    round_trip_live(port, b"POST /warmup HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n");

    // Well past DEFAULT_MAX_REQUEST_BYTES (1 MiB) — a buffered handler would
    // `413` this; the streaming handler takes it incrementally.
    let body_len = 2 * 1024 * 1024 + 7;
    let mut request =
        format!("POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: {body_len}\r\n\r\n").into_bytes();
    request.resize(request.len() + body_len, b'a');

    let response = round_trip(port, &request);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert_eq!(body_of(&response), format!("received:{body_len}"), "the streamed byte count round-trips");
}

/// A `Transfer-Encoding: chunked` upload to a streaming handler is accepted and
/// decoded incrementally (not `411`), the hand-rolled chunked decoder
/// reassembling the body across frames (ADR-0128).
#[test]
fn chunked_upload_streams_to_streaming_handler() {
    let chassis = boot_request_stream::<StreamingUploadHandler>(4);
    let port = port_of(&chassis);

    // "hello" (5) + " world" (6) = 11 body bytes across two chunks.
    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(
        port,
        b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n\
          5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "chunked upload accepted, not 411: {response:?}");
    assert_eq!(body_of(&response), "received:11", "the two chunks decoded to 11 body bytes");
}

/// A request carrying both `Content-Length` and `Transfer-Encoding` (the
/// request-smuggling shape) is refused `411` even for a streaming handler —
/// ADR-0128 relaxes the guard only for a *lone* `chunked` coding, never the
/// ambiguous pair.
#[test]
fn content_length_with_transfer_encoding_is_411() {
    let chassis = boot_request_stream::<StreamingUploadHandler>(4);
    let port = port_of(&chassis);

    let response = round_trip(
        port,
        b"POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\nhello",
    );
    assert!(
        response.starts_with("HTTP/1.1 411 "),
        "smuggling shape stays 411 even for a streaming handler: {response:?}",
    );
}

/// A websocket-upgrade request carrying a lone `Transfer-Encoding: chunked`
/// body is answered `411`, even against a *streaming* handler that would
/// otherwise take a lone chunked body on the non-upgrade path
/// (`chunked_upload_streams_to_streaming_handler`) — the upgrade forces
/// buffering (RFC 6455: a handshake carries no body), so there is nothing
/// to buffer under, the sharpest form of the regression.
///
/// Tripwire: on `origin/main` this returns non-411 because the framing
/// reject sits inside `if ws_key.is_none()` and never runs for a valid
/// upgrade handshake.
#[test]
fn chunked_on_ws_upgrade_is_411() {
    let chassis = boot_request_stream::<StreamingUploadHandler>(4);
    let port = port_of(&chassis);

    let response = round_trip(
        port,
        b"GET /ws HTTP/1.1\r\n\
          Host: localhost\r\n\
          Upgrade: websocket\r\n\
          Connection: Upgrade\r\n\
          Sec-WebSocket-Version: 13\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
          Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 411 "), "expected 411, got: {response:?}");
}

/// A buffered handler (no `HttpRequestStreamOpen` in its accept-set) keeps the
/// unchanged `HttpServerRequest` round trip — the streaming decision is a
/// per-handler property, so an ordinary `Content-Length` POST to
/// [`EchoHttpHandler`] still buffers and echoes verbatim (ADR-0128). The
/// contrast case (`transfer_encoding_is_411`) shows the same handler cannot
/// take a chunked body.
#[test]
fn buffered_handler_keeps_the_unstreamed_path() {
    let chassis = boot_request_stream::<EchoHttpHandler>(4);
    let port = port_of(&chassis);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(port, b"POST /submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert_eq!(body_of(&response), "hello", "buffered body echoed verbatim");
}

/// Boot the server (first, so the routed handlers' `wire` registrations
/// find its mailbox live) with [`FixedBodyHttpHandler`] as the `/`
/// catch-all (its `wire` binds `prefix: "/"`), then the given routed
/// handlers.
macro_rules! routed_chassis {
    ($($handler:ty),+ $(,)?) => {{
        let (registry, mailer) = fresh_substrate();
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<HttpServerCapability>(config_for(1024))
            .with_actor::<FixedBodyHttpHandler>(())
            $(.with_actor::<$handler>(()))+
            .build_passive()
            .expect("caps boot")
    }};
}

/// Poll `request` until its response body equals `expected` (bounded
/// deadline, house pattern — see `tcp/tests.rs` / the bundle
/// `http_serving.rs`). The `wire` route registrations are asynchronous
/// mail, so a route's *first* positive assertion must poll it live
/// rather than race the registration; assertions that depend on an
/// already-proven-live route can then be direct.
fn poll_body(port: u16, request: &[u8], expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = round_trip(port, request);
        if body_of(&response) == expected {
            return;
        }
        assert!(Instant::now() < deadline, "expected body {expected:?} within 10s; last response: {response:?}");
        thread::sleep(Duration::from_millis(25));
    }
}

/// Round-trip `request`, retrying past the pre-registration `503` until the
/// catch-all route's async `wire` mail (ADR-0130) has landed, then return
/// that first non-`503` response. The head/header-asserting sibling of
/// [`poll_body`] (which only matches on the body) for a catch-all route's
/// first positive assertion. None of these handlers legitimately reply
/// `503`, so the first non-`503` is the live response.
fn round_trip_live(port: u16, request: &[u8]) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = round_trip(port, request);
        if !response.starts_with("HTTP/1.1 503 ") {
            return response;
        }
        assert!(Instant::now() < deadline, "route did not go live within 10s; last response: {response:?}");
        thread::sleep(Duration::from_millis(25));
    }
}

/// A `wire`-registered route dispatches as the registered kind — the
/// handler's typed `#[handler]` decodes the request-shaped payload under
/// the minted kind and echoes the path — a deeper path under the claimed
/// prefix is not swallowed but 404s (#3697), and an unrouted path falls
/// back to the `/` catch-all route (ADR-0130).
#[test]
fn routed_prefix_dispatches_as_registered_kind() {
    let chassis = routed_chassis!(ApiRouteHandler);
    let port = port_of(&chassis);

    poll_body(port, b"GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n", "api:/api");

    // A deeper path under the claimed prefix is not swallowed (#3697): the cap
    // routes it to the `/api` dispatcher, which matches exactly and 404s rather
    // than absorbing the deeper path.
    let deeper = round_trip(port, b"GET /api/widgets HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(deeper.starts_with("HTTP/1.1 404 "), "an exact route does not swallow a deeper path: {deeper:?}");

    // A path under no claimed prefix falls back to the `/` catch-all — its own
    // async registration, so poll it live.
    poll_body(port, b"GET /other HTTP/1.1\r\nHost: localhost\r\n\r\n", "fixed body");
}

/// A macro-authored route with a real `FromRequest` extractor dispatches
/// the routed method with the extracted value on success, and — when the
/// extractor returns `Err` — replies that response without ever calling
/// the handler (ADR-0131's typed `400` boundary), both over the wire.
#[test]
fn routed_extractor_success_and_failure() {
    let chassis = routed_chassis!(ExtractRouteHandler);
    let port = port_of(&chassis);

    // Success: the extracted `name` reaches the handler and is echoed.
    poll_body(port, b"GET /extract?name=ada HTTP/1.1\r\nHost: localhost\r\n\r\n", "hello:ada");

    // Failure: with the route proven live, a request missing `name`
    // short-circuits to the extractor's 400 response body.
    let missing = round_trip(port, b"GET /extract HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(missing.starts_with("HTTP/1.1 400 "), "extractor Err becomes the reply status: {missing:?}");
    assert_eq!(body_of(&missing), "missing name query parameter", "extractor Err body is replied verbatim");
}

/// ADR-0154 path templates end to end: nested routes sharing the `/books`
/// static head dispatch by segment + method, `{id}` binds through
/// `Path<u64>`, a non-numeric id is the `FromPathSegment` `400`, and a
/// path the group has no exact template for is a `404`.
#[test]
fn path_template_routes_dispatch_and_capture() {
    let chassis = routed_chassis!(BookRouteHandler);
    let port = port_of(&chassis);

    // Collection and captured-member routes share one (Get, /books) group;
    // `/books` and `/books/{id}` each match their exact segment count, so a
    // member request selects the capture route.
    poll_body(port, b"GET /books HTTP/1.1\r\nHost: localhost\r\n\r\n", "books:list");
    poll_body(port, b"GET /books/42 HTTP/1.1\r\nHost: localhost\r\n\r\n", "books:get:42");

    // The sibling POST group under the same static head, with the capture.
    poll_body(port, b"POST /books/7/checkout HTTP/1.1\r\nHost: localhost\r\n\r\n", "books:checkout:7");

    // A non-numeric capture short-circuits to the FromPathSegment 400
    // rather than falling through to the `/books` prefix.
    let bad = round_trip(port, b"GET /books/notanumber HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(bad.starts_with("HTTP/1.1 400 "), "non-numeric id is a 400: {bad:?}");

    // The POST group has only the exact `/books/{id}/checkout` template and
    // no bare-prefix fallback, so a POST the group can't match is a 404.
    let miss = round_trip(port, b"POST /books/7 HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(miss.starts_with("HTTP/1.1 404 "), "no exact template match is a 404: {miss:?}");
}

/// ADR-0154 §2 deferred routes end to end (relay pattern): `GET /echo`
/// forwards its request to a peer by type via `defer::<EchoPeer>` — an
/// inherited `send_with_context` that keeps the request's chain open — and
/// the reply route answers on the peer's `EchoSay`. `GET /blackhole` forwards
/// to a peer that settles without replying, so the request's chain settles
/// response-less and the server's own `502` net answers.
#[test]
fn deferred_route_forwards_and_answers_on_reply() {
    let chassis = routed_chassis!(DeferRouteHandler, EchoPeer, SilentPeer);
    let port = port_of(&chassis);

    // The reply arrives from the peer and the reply route answers the held
    // request; poll it live past the async route registration.
    poll_body(port, b"GET /echo HTTP/1.1\r\nHost: localhost\r\n\r\n", "echoed:hi");

    // The silent peer settles its inbound without replying, so the request's
    // chain settles response-less and the server answers `502`. Before the
    // `/blackhole` registration lands the path takes the `/` catch-all (a
    // 200), so poll until the 502 appears.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = round_trip(port, b"GET /blackhole HTTP/1.1\r\nHost: localhost\r\n\r\n");
        if response.starts_with("HTTP/1.1 502 ") {
            break;
        }
        assert!(Instant::now() < deadline, "expected a 502 within 10s; last response: {response:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Longest registered prefix wins among overlapping routes (an exact
/// `/api/v2` request beats the `/api` route), matching stops at segment
/// boundaries (`/apiary` is not under `/api`), and a deeper path under the
/// winning prefix is not swallowed — it 404s at that dispatcher (#3697,
/// ADR-0130).
#[test]
fn longest_prefix_wins_on_segment_boundaries() {
    let chassis = routed_chassis!(ApiRouteHandler, ApiV2Handler);
    let port = port_of(&chassis);

    // The exact `/api` hit routes to the `/api` handler.
    poll_body(port, b"GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n", "api:/api");

    // Longest registered prefix wins: `/api/v2` beats `/api` for an exact
    // `/api/v2` request (had `/api` won, its exact route would not match the
    // deeper path and would 404 — so `api-v2` proves `/api/v2` was selected).
    poll_body(port, b"GET /api/v2 HTTP/1.1\r\nHost: localhost\r\n\r\n", "api-v2");

    // A deeper path under the winning prefix is not swallowed (#3697): it
    // routes to the `/api/v2` dispatcher, which matches exactly and 404s.
    let deeper = round_trip(port, b"GET /api/v2/x HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(deeper.starts_with("HTTP/1.1 404 "), "exact route does not swallow a deeper path: {deeper:?}");

    // `/apiary` is not under `/api` (segment boundary), so it takes the `/`
    // catch-all; poll it live (its own async registration) before asserting.
    poll_body(port, b"GET /apiary HTTP/1.1\r\nHost: localhost\r\n\r\n", "fixed body");
}

/// A method-specific route beats a method-agnostic one at equal
/// prefix; other methods take the agnostic route (ADR-0130).
#[test]
fn method_specific_route_beats_agnostic() {
    let chassis = routed_chassis!(MethodPostHandler, MethodAnyHandler);
    let port = port_of(&chassis);

    // Each route's first positive assertion polls it live; together
    // they then pin the precedence (POST → specific, GET → agnostic).
    poll_body(port, b"POST /m HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n", "post-m");
    poll_body(port, b"GET /m HTTP/1.1\r\nHost: localhost\r\n\r\n", "any-m");

    let specific = round_trip(port, b"POST /m HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(body_of(&specific), "post-m", "POST takes the method-specific route with both routes live");
}

/// A bare `#[http::router]` impl still registers exclusive (issue 2625
/// regression guard on the default): two instances of the same compiled
/// actor claim `/macro-excl` through the typed macro surface with no
/// `shared` argument. Because they share the same macro-minted kind, an
/// accidental `shared: true` default would let both instances serve; the
/// exclusive default keeps the route owned by exactly one instance.
#[test]
fn bare_router_stays_exclusive_second_claim_rejected() {
    let chassis = boot_single_shard_fixed_body();
    chassis
        .spawn_actor::<ExclusiveMacroPoolHandler>(Subname::Named("alpha"), b"excl-macro-alpha")
        .finish()
        .expect("spawn alpha");
    chassis
        .spawn_actor::<ExclusiveMacroPoolHandler>(Subname::Named("beta"), b"excl-macro-beta")
        .finish()
        .expect("spawn beta");
    let port = port_of(&chassis);

    let deadline = Instant::now() + Duration::from_secs(10);
    let owner = loop {
        let contested = round_trip(port, b"GET /macro-excl HTTP/1.1\r\nHost: localhost\r\n\r\n");
        match body_of(&contested) {
            "excl-macro-alpha" | "excl-macro-beta" => break body_of(&contested).to_string(),
            _ => {
                assert!(Instant::now() < deadline, "expected /macro-excl to become live within 10s");
                thread::sleep(Duration::from_millis(25));
            }
        }
    };

    for _ in 0..24 {
        let contested = round_trip(port, b"GET /macro-excl HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let body = body_of(&contested);
        assert_eq!(body, owner, "bare #[http::router] must stay exclusive; observed a second route member");
        thread::sleep(Duration::from_millis(10));
    }
}

/// `#[http::router(shared)]` (issue 2625) threads the flag from the
/// attribute all the way into the wire `RegisterRouteSelf` send: two
/// instances of a component built with the opt-in join one round-robin
/// member set and both serve — the bug this catches is the flag failing
/// to thread, which would make the second instance's registration a
/// conflict `Err` instead of a join (only one tag would ever serve).
#[test]
fn macro_router_shared_opt_in_joins_a_member_set() {
    // Pinned to one dispatch shard, like `shared_route_spreads_across_members`:
    // round-robin state is per-shard, so alternation across a request
    // sequence is only deterministic with a single shard.
    let chassis = boot_single_shard_fixed_body();
    // Two named instances of the exact same `SharedMacroPoolHandler` type
    // (the accurate replica analog, per the type's own doc comment): each
    // instance's `wire` runs the identical macro-emitted `shared: true`
    // registration, so both carry the same minted `Kind::ID` and can join
    // one member set.
    chassis
        .spawn_actor::<SharedMacroPoolHandler>(Subname::Named("alpha"), b"macro-alpha")
        .finish()
        .expect("spawn alpha");
    chassis.spawn_actor::<SharedMacroPoolHandler>(Subname::Named("beta"), b"macro-beta").finish().expect("spawn beta");
    let port = port_of(&chassis);

    // Wait until both registrations are live: with the set complete a
    // pair of consecutive requests serves both bodies.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let first = round_trip(port, b"GET /macro-pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let second = round_trip(port, b"GET /macro-pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let pair = [body_of(&first).to_string(), body_of(&second).to_string()];
        if pair.contains(&"macro-alpha".to_string()) && pair.contains(&"macro-beta".to_string()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expected macro-alpha+macro-beta across a request pair within 10s; got {pair:?}",
        );
        thread::sleep(Duration::from_millis(25));
    }

    // Steady state: six more requests keep alternating — both members
    // serve, and only members serve.
    let mut alpha = 0;
    let mut beta = 0;
    for _ in 0..6 {
        let response = round_trip(port, b"GET /macro-pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        match body_of(&response) {
            "macro-alpha" => alpha += 1,
            "macro-beta" => beta += 1,
            other => panic!("unexpected /macro-pool body {other:?}"),
        }
    }
    assert_eq!((alpha, beta), (3, 3), "round-robin alternation over 6 requests");
}

/// A peer that stalls its receive window blocks only its own reader
/// thread, never the dispatch shard (ADR-0135 §3): with one shard
/// pinned, connection A parks a 16 MiB echo response against a client
/// that refuses to read while connection B's small request round-trips
/// promptly through the same shard.
///
/// Tripwire: with the response write back on the shard's dispatch (the
/// pre-ADR-0135 §3 shape), A's blocked `write_all` freezes the shard
/// and B times out empty.
#[test]
fn stalled_peer_does_not_block_sibling_connections() {
    let chassis = boot_chassis::<EchoHttpHandler>(HttpServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        // Short: the stalled write parks within milliseconds, and
        // teardown waits out at most one response deadline.
        request_timeout_millis: 2_000,
        max_request_bytes: 32 * 1024 * 1024,
        dispatch_shards: 1,
        ..HttpServerConfig::default()
    });
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the stall setup, so both
    // connections dispatch to the echo handler rather than racing it.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    // Connection A: a 16 MiB echo whose response the client never
    // reads — far past loopback socket buffering, so the reader's
    // write_all parks against A's receive window.
    let body_len = 16 * 1024 * 1024;
    let mut stalled = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect stalled peer");
    let mut request =
        format!("POST /stall HTTP/1.1\r\nHost: localhost\r\nContent-Length: {body_len}\r\n\r\n").into_bytes();
    request.resize(request.len() + body_len, b'a');
    stalled.write_all(&request).expect("write stalled request");
    stalled.flush().expect("flush stalled request");

    // Give the echo time to dispatch and its response write to park.
    thread::sleep(Duration::from_millis(300));

    // Connection B on the same (sole) shard round-trips promptly.
    let response = round_trip(port, b"GET /probe HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(
        response.starts_with("HTTP/1.1 200 "),
        "sibling connection must be served while a peer stalls; got: {response:?}",
    );

    drop(stalled);
}

/// A route registered mid-connection is visible to the very next
/// request on an already-kept-alive socket (ADR-0135 §2): the reader
/// re-reads the shared route table per request head, so registration
/// granularity is next-request, not next-connection.
///
/// Tripwire: a reader-side route *snapshot* taken at connection
/// adoption would serve the catch-all forever on a long-lived
/// connection; this test's second-phase request would never flip to
/// the routed body.
///
/// The `/late` target is [`WiredRouteHandler`] (its generic `on_extra`
/// serves `HttpServerRequest` and its `wire` claims only `/wired…`,
/// never `/`), so the sole `/` catch-all here is [`EchoHttpHandler`] —
/// two handlers both claiming `/` would be a registration conflict.
#[test]
fn route_registered_mid_connection_serves_next_request() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<WiredRouteHandler>(())
        .with_actor::<HttpServerCapability>(keep_alive_config_for(5_000))
        .build_passive()
        .expect("caps boot");
    let port = port_of(&chassis);

    // Poll the echo `/` catch-all live before the pre-registration read.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    let mut carry = Vec::new();

    // Pre-registration: /late takes the echo `/` catch-all.
    stream.write_all(b"GET /late HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write request");
    let first = read_one_response(&mut stream, &mut carry);
    assert!(first.contains("x-aether-path: /late"), "pre-registration request takes the echo catch-all: {first:?}");

    // Register /late at the wired handler while the connection is
    // parked between keep-alive requests.
    let supervisor = registry.lookup(<HttpServerCapability as Addressable>::NAMESPACE).expect("http server registered");
    let target = registry.lookup(<WiredRouteHandler as Addressable>::NAMESPACE).expect("wired handler registered");
    let payload = RegisterRoute {
        prefix: "/late".to_string(),
        method: None,
        kind: <RequestKind as KindTrait>::ID,
        mailbox: target,
        shared: false,
    }
    .encode_into_bytes();
    mailer.push(Mail::new(supervisor, KindId(<RegisterRoute as KindTrait>::ID.0), payload, 1));

    // The registration lands asynchronously; poll on the SAME socket.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        stream.write_all(b"GET /late HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write request");
        let response = read_one_response(&mut stream, &mut carry);
        if body_of(&response) == "wired-raw" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mid-connection registration should reach the next request within 10s; \
             last: {response:?}",
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// A shared member set (ADR-0136) spreads requests across its members
/// round-robin: alpha and beta both register `/pool` shared, and with
/// `dispatch_shards` pinned to 1 (one cursor) sequential requests
/// alternate between them — both bodies observed, nothing else.
///
/// Tripwire: without member sets the second shared claim is rejected
/// and every request serves "alpha"; with a broken cursor (never
/// advancing) likewise.
#[test]
fn shared_route_spreads_across_members() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<HttpServerCapability>(HttpServerConfig {
            bind_addr: "127.0.0.1:0".to_string(),
            request_timeout_millis: 5_000,
            dispatch_shards: 1,
            ..HttpServerConfig::default()
        })
        .with_actor::<FixedBodyHttpHandler>(())
        .with_actor::<SharedAlphaHandler>(())
        .with_actor::<SharedBetaHandler>(())
        .build_passive()
        .expect("caps boot");
    let port = port_of(&chassis);

    // Wait until both registrations are live: with the set complete a
    // pair of consecutive requests serves both bodies.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let first = round_trip(port, b"GET /pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let second = round_trip(port, b"GET /pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let pair = [body_of(&first).to_string(), body_of(&second).to_string()];
        if pair.contains(&"alpha".to_string()) && pair.contains(&"beta".to_string()) {
            break;
        }
        assert!(Instant::now() < deadline, "expected alpha+beta across a request pair within 10s; got {pair:?}");
        thread::sleep(Duration::from_millis(25));
    }

    // Steady state: six more requests keep alternating — both members
    // serve, and only members serve.
    let mut alpha = 0;
    let mut beta = 0;
    for _ in 0..6 {
        let response = round_trip(port, b"GET /pool HTTP/1.1\r\nHost: localhost\r\n\r\n");
        match body_of(&response) {
            "alpha" => alpha += 1,
            "beta" => beta += 1,
            other => panic!("unexpected /pool body {other:?}"),
        }
    }
    assert_eq!((alpha, beta), (3, 3), "round-robin alternation over 6 requests");
}

/// A macro route composes with a hand-written `wire`: the macro appends
/// its `/wired` registration to the author's `wire` without displacing
/// the raw `/wired-extra` claim already there, so both dispatch (ADR-0131
/// append path).
#[test]
fn hand_written_wire_and_macro_route_compose() {
    let chassis = routed_chassis!(WiredRouteHandler);
    let port = port_of(&chassis);

    // The macro-appended registration reaches the cap.
    poll_body(port, b"GET /wired HTTP/1.1\r\nHost: localhost\r\n\r\n", "wired-macro");
    // The author's own `wire` registration survived the append.
    poll_body(port, b"GET /wired-extra HTTP/1.1\r\nHost: localhost\r\n\r\n", "wired-raw");
}

/// `unregister_route_self` releases the sender's route: the first
/// request reaches the routed handler (which releases the route while
/// answering), and a subsequent request falls back. The release is a
/// separate mail racing the reply, so the fallback is asserted with a
/// bounded poll rather than a single follow-up request.
#[test]
fn self_unregister_releases_route() {
    let chassis = routed_chassis!(TmpRouteHandler);
    let port = port_of(&chassis);

    // Poll the route live (a pre-registration request harmlessly falls
    // back without triggering the handler's release); the first "tmp"
    // response is also the one that releases the route.
    poll_body(port, b"GET /tmp HTTP/1.1\r\nHost: localhost\r\n\r\n", "tmp");

    // The release is a separate mail racing the reply, so the fallback
    // is asserted with the same bounded poll.
    poll_body(port, b"GET /tmp HTTP/1.1\r\nHost: localhost\r\n\r\n", "fixed body");
}

/// Server config with a short idle (keep-alive) timeout, for the
/// idle-close test — every other field matches [`config_for`].
fn keep_alive_config_for(keep_alive_timeout_millis: u64) -> HttpServerConfig {
    HttpServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        keep_alive_timeout_millis,
        ..HttpServerConfig::default()
    }
}

/// Read exactly one HTTP/1.1 response off `stream` — the head up to its
/// blank line, then its `Content-Length` body — leaving any bytes read past
/// it (a pipelined next response) in `carry` for the following call. Panics
/// on EOF mid-response, so a test that expects the connection to close reads
/// one response and then asserts EOF separately.
/// Read from `stream` into `carry` until the blank line terminating the
/// HTTP response head is buffered; return the byte index just past it.
/// Shared by the buffered and chunked response readers.
fn read_response_head(stream: &mut TcpStream, carry: &mut Vec<u8>, chunk: &mut [u8]) -> usize {
    loop {
        if let Some(pos) = carry.windows(4).position(|window| window == b"\r\n\r\n") {
            return pos + 4;
        }
        let n = stream.read(chunk).expect("read response head");
        assert!(n > 0, "eof before response head; buffered: {:?}", String::from_utf8_lossy(carry));
        carry.extend_from_slice(&chunk[..n]);
    }
}

fn read_one_response(stream: &mut TcpStream, carry: &mut Vec<u8>) -> String {
    let mut chunk = [0u8; 4096];
    let head_end = read_response_head(stream, carry, &mut chunk);
    let content_length = content_length_of(&carry[..head_end]);
    while carry.len() < head_end + content_length {
        let n = stream.read(&mut chunk).expect("read response body");
        assert!(n > 0, "eof mid response body");
        carry.extend_from_slice(&chunk[..n]);
    }
    let response = String::from_utf8_lossy(&carry[..head_end + content_length]).into_owned();
    carry.drain(..head_end + content_length);
    response
}

/// Parse the `Content-Length` from a response head (case-insensitive), `0`
/// when absent.
fn content_length_of(head: &[u8]) -> usize {
    let text = String::from_utf8_lossy(head);
    for line in text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// Read exactly one *chunked* transfer-encoding HTTP/1.1 response off
/// `stream` (a streamed response, ADR-0128) — the head up to its blank line,
/// then the chunked body up to and including its terminating zero-length
/// chunk (`0\r\n\r\n`) — leaving any bytes read past it (a pipelined next
/// response) in `carry` for the following call. The chunked mirror of
/// [`read_one_response`], which only handles the buffered `Content-Length`
/// case.
fn read_one_chunked_response(stream: &mut TcpStream, carry: &mut Vec<u8>) -> String {
    let mut chunk = [0u8; 4096];
    let head_end = read_response_head(stream, carry, &mut chunk);
    let terminator = b"0\r\n\r\n";
    let body_end = loop {
        if let Some(pos) = carry[head_end..].windows(terminator.len()).position(|window| window == terminator) {
            break head_end + pos + terminator.len();
        }
        let n = stream.read(&mut chunk).expect("read chunked response body");
        assert!(n > 0, "eof mid chunked response body");
        carry.extend_from_slice(&chunk[..n]);
    };
    let response = String::from_utf8_lossy(&carry[..body_end]).into_owned();
    carry.drain(..body_end);
    response
}

/// Assert that the next read on `stream` observes the server's close (EOF).
/// The stream's read timeout bounds this so a server that failed to close
/// surfaces as a timeout rather than a hang.
fn assert_closed(stream: &mut TcpStream) {
    let mut tail = [0u8; 64];
    let read = stream.read(&mut tail);
    assert!(matches!(read, Ok(0)), "expected the server to close the connection, got: {read:?}");
}

/// Two requests round-trip in order on one kept-alive socket (HTTP/1.1
/// default, no `Connection: close`), each response carrying `Connection:
/// keep-alive`; a final `Connection: close` request then terminates the
/// connection. The two requests are written pipelined (both before the first
/// response is read), so this also exercises the reader carrying request 2's
/// over-read bytes across the resume signal.
#[test]
fn keep_alive_serves_sequential_requests_on_one_socket() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the pipelined round trip.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    let mut carry: Vec<u8> = Vec::new();

    // Pipeline both requests, then read both responses in order.
    stream
        .write_all(
            b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\n\
              GET /two HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .expect("write pipelined requests");
    stream.flush().expect("flush");

    let first = read_one_response(&mut stream, &mut carry);
    assert!(first.starts_with("HTTP/1.1 200 OK\r\n"), "first response 200: {first:?}");
    assert!(first.contains("x-aether-path: /one\r\n"), "first response is /one: {first:?}");
    assert!(first.contains("Connection: keep-alive\r\n"), "first response keeps alive: {first:?}");

    let second = read_one_response(&mut stream, &mut carry);
    assert!(second.contains("x-aether-path: /two\r\n"), "second response is /two, in order: {second:?}");
    assert!(second.contains("Connection: keep-alive\r\n"), "second response keeps alive: {second:?}");

    // A final `Connection: close` request terminates the connection.
    stream
        .write_all(b"GET /three HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write closing request");
    stream.flush().expect("flush");
    let third = read_one_response(&mut stream, &mut carry);
    assert!(third.contains("x-aether-path: /three\r\n"), "third response is /three: {third:?}");
    assert!(third.contains("Connection: close\r\n"), "third response closes: {third:?}");
    assert_closed(&mut stream);
}

/// Tripwire (issue #2582): a streamed (chunked) response to a keep-alive
/// request renders `Connection: keep-alive` and the socket is reused for a
/// second request — the streamed mirror of
/// [`keep_alive_serves_sequential_requests_on_one_socket`]. Before the fix,
/// `render_stream_head` hardcoded `Connection: close` and `finish_stream`
/// unconditionally closed the connection, so this second read would hang /
/// fail against the pre-fix behavior.
#[test]
fn keep_alive_reuses_socket_after_streamed_response() {
    let chassis = boot_response_stream::<StreamHttpHandler>(8);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the persistent-socket reads.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    let mut carry: Vec<u8> = Vec::new();

    let expected: Vec<u8> = (0..STREAM_CHUNK_COUNT).flat_map(stream_chunk_body).collect();

    // First streamed request, HTTP/1.1 default (no `Connection: close`).
    stream.write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write first request");
    stream.flush().expect("flush");
    let first = read_one_chunked_response(&mut stream, &mut carry);
    assert!(first.starts_with("HTTP/1.1 200 OK\r\n"), "{first:?}");
    assert!(first.contains("Connection: keep-alive\r\n"), "streamed response keeps alive: {first:?}");
    assert_eq!(dechunk(body_of(&first)).into_bytes(), expected, "first stream reassembles in order");

    // The reuse invariant: a second request on the same socket after the
    // stream ended gets served rather than the socket being closed.
    stream.write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write second request");
    stream.flush().expect("flush");
    let second = read_one_chunked_response(&mut stream, &mut carry);
    assert!(second.starts_with("HTTP/1.1 200 OK\r\n"), "{second:?}");
    assert!(second.contains("Connection: keep-alive\r\n"), "second streamed response keeps alive too: {second:?}");
    assert_eq!(dechunk(body_of(&second)).into_bytes(), expected, "second stream reassembles in order");
}

/// Tripwire: two consecutive response streams must carry distinct
/// `stream_id`s. The id is the key of the cap's stream table and of the
/// credit accounting that guards it, so a repeated id lets one stream's
/// credit grants and teardown act on another's state — with the id constant,
/// a stale grant is byte-indistinguishable from a fresh one and drives a
/// correct handler past its window into the over-window teardown (issue
/// 3730). Response streams once reused the dispatch correlation id, which is
/// minted per sender and so repeated across requests; ADR-0128 §2 was amended
/// on 2026-07-20 to mint them from the cap's monotonic counter instead. The
/// handler echoes its own granted `stream_id` as the body, so this reads the
/// two ids off the wire and only asserts they differ — never their values,
/// which would pin the counter's start point.
#[test]
fn consecutive_response_streams_get_distinct_stream_ids() {
    let chassis = boot_response_stream::<StreamIdEchoHandler>(8);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the measured requests.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let first = round_trip(port, b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let second = round_trip(port, b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let first_id = dechunk(body_of(&first));
    let second_id = dechunk(body_of(&second));

    assert!(!first_id.is_empty(), "first stream reported no id: {first:?}");
    assert!(!second_id.is_empty(), "second stream reported no id: {second:?}");
    assert_ne!(first_id, second_id, "consecutive response streams reused stream_id {first_id}");
}

/// Pins the negative alongside the reuse tripwire above: a streamed request
/// carrying `Connection: close` still tears the socket down once the stream
/// ends, exactly like the buffered path.
#[test]
fn streamed_response_honors_explicit_connection_close() {
    let chassis = boot_response_stream::<StreamHttpHandler>(8);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the explicit-close read.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    let mut carry: Vec<u8> = Vec::new();

    stream.write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").expect("write request");
    stream.flush().expect("flush");
    let response = read_one_chunked_response(&mut stream, &mut carry);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert!(response.contains("Connection: close\r\n"), "streamed response honors explicit close: {response:?}");
    assert_closed(&mut stream);
}

/// An HTTP/1.0 request with no `Connection` header closes by default: the
/// response carries `Connection: close` and the server closes the socket.
#[test]
fn http_1_0_defaults_to_close() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the HTTP/1.0 read.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    stream.write_all(b"GET /ten HTTP/1.0\r\nHost: localhost\r\n\r\n").expect("write request");
    stream.flush().expect("flush");

    let mut carry: Vec<u8> = Vec::new();
    let response = read_one_response(&mut stream, &mut carry);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "expected 200, got: {response:?}");
    assert!(response.contains("Connection: close\r\n"), "HTTP/1.0 defaults to close: {response:?}");
    assert_closed(&mut stream);
}

/// A kept-alive connection left idle between requests is closed by the
/// server after the configured `keep_alive_timeout_millis`, rather than
/// pinning the reader thread for the full request timeout.
#[test]
fn idle_kept_alive_connection_closes_after_timeout() {
    let chassis = boot_chassis::<EchoHttpHandler>(keep_alive_config_for(300));
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the kept-alive read.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    stream.write_all(b"GET /keep HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write request");
    stream.flush().expect("flush");

    let mut carry: Vec<u8> = Vec::new();
    let response = read_one_response(&mut stream, &mut carry);
    assert!(response.contains("Connection: keep-alive\r\n"), "kept-alive response: {response:?}");

    // Now idle. The 300 ms idle timeout closes the connection well before the
    // 5 s request timeout / read timeout would — the elapsed bound is the
    // tripwire distinguishing the idle close from a slow read timeout.
    let started = Instant::now();
    assert_closed(&mut stream);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "idle connection closed via the keep-alive timeout, not the request timeout",
    );
}
