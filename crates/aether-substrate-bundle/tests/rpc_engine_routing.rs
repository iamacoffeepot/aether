// End-to-end test for the hub's `engine = Some(_)` RPC routing
// (issue 763 P5a).
//
// Boots a "hub" chassis (forwarding `RpcServerCapability` + the
// `aether.engine` engines cap), connects a raw RPC client to it, and
// drives the whole forward model through that one socket — exactly
// the shape the out-of-process `aether-mcp` binary will take in P5d:
//
//   1. An `engine = None` Call spawns a real `aether-substrate-headless`
//      via the engines cap and yields its `engine_id`.
//   2. An `engine = Some(engine_id)` Call is *routed* — hub RpcServer
//      -> `aether.engine` -> proxy -> (RPC) -> substrate -> back — and
//      the substrate's reply streams home as `ReplyEvent` + `ReplyEnd`.
//   3. An `engine = None` `TerminateEngine` Call cleans the engine up.
//
// Step 2 is the P5a proof: before this phase, `engine = Some` Calls
// were rejected with `RpcError::UnsupportedTarget`.

use aether_capabilities::EngineServer;
use aether_capabilities::rpc::{
    Hello, HelloAck, MailEnvelope, MailboxAddress, PeerKind, RpcServerCapability, RpcServerConfig,
    WIRE_VERSION, WireFrame,
};
use aether_capabilities::trace::TraceObserverCapability;
use aether_codec::frame::{read_frame, write_frame};
use aether_data::{EngineId, Kind, Uuid, mailbox_id_from_name};
use aether_kinds::{List, ListResult, SpawnEngine, SpawnEngineResult, TerminateEngine};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis, NeverDriver, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::handle_store::HandleStore;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

struct TestChassis;
impl Chassis for TestChassis {
    const PROFILE: &'static str = "test";
    type Driver = NeverDriver;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("TestChassis is driven by Builder::new directly in this test")
    }
}

/// Boot a hub-shaped passive chassis: a forwarding `RpcServerCapability`
/// (engine-addressed Calls route through `aether.engine`), the engines
/// cap, and `TraceObserverCapability` so the RpcServer's local Calls
/// (`spawn`, `terminate`) settle and close.
fn boot_hub() -> (PassiveChassis<TestChassis>, u16) {
    let registry = Arc::new(Registry::new());
    for d in aether_kinds::descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let store = Arc::new(HandleStore::new(1024 * 1024));
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceObserverCapability>(())
        .with_actor::<EngineServer>(())
        .with_actor::<RpcServerCapability>(RpcServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            peer_kind: PeerKind::Substrate {
                engine_name: "test-hub".into(),
                engine_version: "0.1.0".into(),
                kinds: vec![],
            },
        })
        .build_passive()
        .expect("hub caps boot");
    let port = chassis
        .handle::<aether_capabilities::rpc::RpcServerHandle>()
        .expect("RpcServerHandle published")
        .local_port;
    (chassis, port)
}

/// Fire one `Call` and read frames until its `ReplyEnd` arrives,
/// returning the `(kind, payload)` of the single `ReplyEvent` seen in
/// between (these calls each yield exactly one event then end). Panics
/// on a `ReplyEnd::Err` or a missing event.
fn call_round_trip<K: Kind + serde::Serialize>(
    stream: &mut TcpStream,
    cid: u64,
    engine: Option<EngineId>,
    mailbox_name: &str,
    request: &K,
) -> (aether_data::KindId, Vec<u8>) {
    write_frame(
        stream,
        &WireFrame::Call {
            cid: Some(cid),
            envelope: MailEnvelope {
                to: MailboxAddress {
                    engine,
                    mailbox: mailbox_id_from_name(mailbox_name),
                },
                from: None,
                kind: K::ID,
                correlation_id: None,
                payload: request.encode_into_bytes(),
            },
        },
    )
    .expect("write Call");

    let mut event: Option<(aether_data::KindId, Vec<u8>)> = None;
    loop {
        match read_frame(stream).expect("read reply frame") {
            WireFrame::ReplyEvent {
                cid: got_cid,
                envelope,
            } => {
                assert_eq!(got_cid, cid, "ReplyEvent cid mismatch");
                event = Some((envelope.kind, envelope.payload));
            }
            WireFrame::ReplyEnd {
                cid: got_cid,
                result,
            } => {
                assert_eq!(got_cid, cid, "ReplyEnd cid mismatch");
                result.unwrap_or_else(|e| panic!("call {cid} ended with error: {e:?}"));
                return event.unwrap_or_else(|| panic!("call {cid} ended with no ReplyEvent"));
            }
            other => panic!("unexpected frame for call {cid}: {other:?}"),
        }
    }
}

#[test]
fn hub_routes_engine_addressed_calls_to_a_real_substrate() {
    let (_chassis, hub_port) = boot_hub();
    let headless = env!("CARGO_BIN_EXE_aether-substrate-headless");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{hub_port}")).expect("connect to hub");
    // Generous: a forwarded call's first step is forking a real
    // substrate and waiting for it to bind its RPC port.
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();

    // Handshake.
    write_frame(
        &mut stream,
        &WireFrame::Hello(Hello {
            wire_version: WIRE_VERSION,
            peer: PeerKind::Client {
                client_name: "rpc-engine-routing-test".into(),
                client_version: "0.0.1".into(),
            },
        }),
    )
    .expect("write Hello");
    match read_frame(&mut stream).expect("read HelloAck") {
        WireFrame::HelloAck(HelloAck { wire_version, .. }) => {
            assert_eq!(wire_version, WIRE_VERSION);
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }

    // 1. engine = None: spawn a real headless substrate via the
    //    hub-local engines cap. Dispatches locally on the hub.
    let (spawn_kind, spawn_payload) = call_round_trip(
        &mut stream,
        1,
        None,
        "aether.engine",
        &SpawnEngine {
            binary_path: headless.to_owned(),
            args: vec![],
        },
    );
    assert_eq!(spawn_kind, <SpawnEngineResult as Kind>::ID);
    let engine_id = match SpawnEngineResult::decode_from_bytes(&spawn_payload) {
        Some(SpawnEngineResult::Ok { engine_id, .. }) => engine_id,
        Some(SpawnEngineResult::Err { error }) => panic!("spawn failed: {error}"),
        None => panic!("undecodable SpawnEngineResult"),
    };
    let engine_id = EngineId(Uuid::parse_str(&engine_id).expect("engine_id parses"));

    // 2. engine = Some(_): a ROUTED call. The hub forwards it through
    //    aether.engine -> proxy -> (RPC) -> the substrate's aether.fs
    //    -> back. This is the P5a proof.
    let (routed_kind, _routed_payload) = call_round_trip(
        &mut stream,
        2,
        Some(engine_id),
        "aether.fs",
        &List {
            namespace: "save".to_owned(),
            prefix: String::new(),
        },
    );
    assert_eq!(
        routed_kind,
        <ListResult as Kind>::ID,
        "routed call should return the substrate's aether.fs ListResult",
    );

    // 3. engine = None: terminate the engine — proxy SIGKILLs + reaps
    //    the child, so the test leaves no orphaned substrate.
    let (term_kind, _term_payload) = call_round_trip(
        &mut stream,
        3,
        None,
        "aether.engine",
        &TerminateEngine {
            engine_id: engine_id.0.to_string(),
        },
    );
    assert_eq!(term_kind, <aether_kinds::TerminateEngineResult as Kind>::ID);
}
