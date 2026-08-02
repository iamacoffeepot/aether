//! The shared-route fixtures (ADR-0136 / issue 2625): handlers that claim a
//! prefix `shared: true` and so join a round-robin member set, plus the bare
//! (exclusive-by-default) router the negative case pins.

use aether_actor::actor;
use aether_data::Kind;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate as http;
use crate::kinds::{HttpServerRequest, HttpServerResponse, RegisterRouteSelf};
use crate::server::HttpServerCapability;

/// A routed handler whose `wire` registers each claim `shared: true`
/// with the given dispatch kind (ADR-0136) — the member-set opt-in
/// the shared-route tests exercise. Replies `200` with a fixed tag
/// body.
macro_rules! shared_routed_handler {
    ($ty:ident, $state:ident, $namespace:literal, $tag:literal, $kind:ty,
     [$(($method:expr, $prefix:literal)),+ $(,)?]) => {
        pub struct $ty;
        pub struct $state;

        #[actor(singleton, root)]
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
#[actor(instanced, root)]
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
#[actor(instanced, root)]
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
    fn on_excl(state: &mut ExclusiveMacroPoolHandlerState, _ctx: http::Ctx<'_, NativeCtx<'_>>) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: state.tag.to_vec() }
    }
}
