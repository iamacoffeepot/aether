//! Scheduler-backed ADR-0165 proof for the HTTP consumer migration. The
//! harness is intentionally native-only: no component host or wasm module is
//! composed, so the observed delay is dispatch-shard activation itself.

use aether_actor::actor;
use aether_data::Kind;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_http::{
    HttpServerCapability, HttpServerConfig, HttpServerRequest, HttpServerResponse, RegisterRouteResult,
    RegisterRouteSelf,
};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

struct ColdHttpHandler;
struct ColdHttpHandlerState;

#[actor(singleton, root)]
impl NativeActor for ColdHttpHandler {
    type State = ColdHttpHandlerState;
    type Config = ();

    const NAMESPACE: &'static str = "aether.http.test.staged-shards";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<ColdHttpHandlerState, BootError> {
        Ok(ColdHttpHandlerState)
    }

    fn wire(_state: &mut ColdHttpHandlerState, ctx: &mut NativeCtx<'_>) {
        ctx.actor::<HttpServerCapability>().send(&RegisterRouteSelf {
            prefix: "/".to_owned(),
            method: None,
            kind: <HttpServerRequest as Kind>::ID,
            shared: false,
        });
    }

    #[handler::single]
    fn on_request(
        _state: &mut ColdHttpHandlerState,
        _ctx: &mut NativeCtx<'_>,
        request: HttpServerRequest,
    ) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: request.path.into_bytes() }
    }

    #[handler::single]
    fn on_registered(_state: &mut ColdHttpHandlerState, _ctx: &mut NativeCtx<'_>, _result: RegisterRouteResult) {}
}

fn available_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve staged-shard loopback address");
    let address = listener.local_addr().expect("reserved staged-shard address");
    drop(listener);
    address
}

/// Both client sockets and requests exist before the first explicit harness
/// frame. Advancing crosses a synchronous harness boundary while the real
/// scheduler, registry owner, activation homes, and parent task completions
/// run; the first wave then receives complete responses instead of being
/// posted to merely reserved shards and stranded.
#[test]
fn cold_first_wave_crosses_staged_shard_activation_in_one_frame() {
    let address = available_loopback_addr();
    let mut harness = SubstrateHarness::builder()
        .with_actor_configured::<HttpServerCapability>(
            (),
            HttpServerConfig {
                enabled: true,
                bind_addr: address.to_string(),
                dispatch_shards: 2,
                request_timeout_millis: 5_000,
                ..HttpServerConfig::default()
            },
        )
        .with_actor::<ColdHttpHandler>(())
        .build()
        .expect("boot native-only staged HTTP harness");

    let mut streams = ["/cold-a", "/cold-b"]
        .into_iter()
        .map(|path| {
            let mut stream = TcpStream::connect(address).expect("connect before first harness frame");
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("bound staged response read");
            write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .expect("write cold request before advancing");
            stream.flush().expect("flush cold request before advancing");
            (path, stream)
        })
        .collect::<Vec<_>>();

    harness.execute(vec![("activate", HarnessOp::advance(1))]).expect("advance staged activation frame");

    for (path, stream) in &mut streams {
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response after staged activation");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "cold request completes after activation: {response:?}");
        assert_eq!(response.split_once("\r\n\r\n").map_or("", |(_, body)| body), *path);
    }
}
