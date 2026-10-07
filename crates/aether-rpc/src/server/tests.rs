// The test chassis are deliberately built bare with `Builder::new`, not
// through the based boot path.
#![allow(clippy::disallowed_methods)]
use super::*;
use crate::{Hello, HelloAck, PeerKind, Recipient, WIRE_VERSION, WireFrame};
use aether_actor::{ActorRef, Addressable, Anyone, OutboundReply, Unchecked};
use aether_codec::frame::{FrameError, read_frame, write_frame};
use aether_data::{EngineId, ErasedActorPath, Source, Uuid};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::builder::Builder;
use aether_substrate::chassis::builder::PassiveChassis;
use aether_substrate::chassis::error::BootError;
use aether_substrate::testing::{TestChassis, fresh_substrate};
use aether_trace::TraceDispatchCapability;
use std::collections::VecDeque;
use std::net::TcpStream;
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// An actor path no actor in these test chassis holds.
const ABSENT: &str = "test.rpc.absent";

/// The local recipient naming actor `A` by its namespace path.
fn recipient_of<A: Addressable>() -> Recipient {
    Recipient::local(ErasedActorPath::new(A::NAMESPACE).expect("an actor namespace is a path"))
}

fn test_peer_kind() -> PeerKind {
    PeerKind::Substrate { engine_name: "test".into(), engine_version: "0.1.0".into(), kinds: vec![] }
}

#[aether_data::kind(name = "aether.rpc.test.register_engine_route", copy)]
struct RegisterEngineRouteForTest {
    engine_id: EngineId,
}

#[aether_data::kind(name = "aether.rpc.test.complete_engine_route", copy)]
struct CompleteEngineRouteForTest {
    value: u64,
}

#[aether_data::kind(name = "aether.rpc.test.engine_route_result", copy, eq)]
struct EngineRouteReplyForTest {
    value: u64,
}

#[aether_data::kind(name = "aether.rpc.test.engine_route_barrier", default)]
struct EngineRouteBarrierForTest;

#[aether_data::kind(name = "aether.rpc.test.stop_engine_route", default)]
struct StopEngineRouteForTest;

struct UncheckedEngineRouteConfig {
    registrations: mpsc::Sender<(EngineId, crate::RegisterEngineRouteResult)>,
    forwards: mpsc::Sender<()>,
}

struct UncheckedEngineRoute {
    registrations: mpsc::Sender<(EngineId, crate::RegisterEngineRouteResult)>,
    registration_requests: VecDeque<EngineId>,
    forwards: mpsc::Sender<()>,
    pending: VecDeque<Source>,
}

#[aether_actor::actor(instanced, root, depends(RpcServerCapability))]
impl NativeActor for UncheckedEngineRoute {
    type Config = UncheckedEngineRouteConfig;
    const NAMESPACE: &'static str = "aether.rpc.test.unchecked_engine_route";

    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            registrations: config.registrations,
            registration_requests: VecDeque::new(),
            forwards: config.forwards,
            pending: VecDeque::new(),
        })
    }

    #[handler::tell]
    fn on_register(&mut self, ctx: &mut NativeCtx<'_>, mail: RegisterEngineRouteForTest) {
        self.registration_requests.push_back(mail.engine_id);
        ctx.send::<RpcServerCapability>(&RegisterEngineRoute { engine_id: mail.engine_id });
    }

    #[handler::response]
    fn on_registration_result(&mut self, _ctx: &mut NativeCtx<'_>, mail: crate::RegisterEngineRouteResult) {
        let engine_id = self.registration_requests.pop_front().expect("registration request precedes its result");
        self.registrations.send((engine_id, mail)).expect("registration result receiver stays live");
    }

    #[handler::unchecked(reason = "test: parks the forwarded call for a later completion")]
    fn on_forward(&mut self, ctx: &mut NativeCtx<'_, Self, Anyone, Unchecked>, _mail: crate::ForwardEnvelope) {
        self.pending.push_back(ctx.reply_target());
        self.forwards.send(()).expect("forward observer stays live");
    }

    #[handler::unchecked(reason = "test: completes a parked call from another handler")]
    fn on_complete(&mut self, ctx: &mut NativeCtx<'_, Self, Anyone, Unchecked>, mail: CompleteEngineRouteForTest) {
        let target = self.pending.pop_front().expect("a forwarded call is pending");
        ctx.reply_to(target, &EngineRouteReplyForTest { value: mail.value });
        ctx.reply_to(target, &crate::CallSettled::Ok);
    }

    #[handler::tell]
    fn on_barrier(&mut self, _ctx: &mut NativeCtx<'_>, _mail: EngineRouteBarrierForTest) {
        assert_eq!(self.pending.len(), 1, "the post-forward barrier observes one pending remote call");
    }

    #[handler::tell]
    fn on_stop(&mut self, ctx: &mut NativeCtx<'_>, _mail: StopEngineRouteForTest) {
        assert_eq!(self.pending.len(), 1, "the route stops with the second remote call pending");
        ctx.shutdown();
    }
}

struct WrongEngineRouteConfig {
    registrations: mpsc::Sender<(EngineId, crate::RegisterEngineRouteResult)>,
    forwards: mpsc::Sender<()>,
}

struct WrongEngineRoute {
    registrations: mpsc::Sender<(EngineId, crate::RegisterEngineRouteResult)>,
    registration_requests: VecDeque<EngineId>,
    forwards: mpsc::Sender<()>,
}

#[aether_actor::actor(instanced, root, depends(RpcServerCapability))]
impl NativeActor for WrongEngineRoute {
    type Config = WrongEngineRouteConfig;
    const NAMESPACE: &'static str = "aether.rpc.test.wrong_engine_route";

    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            registrations: config.registrations,
            registration_requests: VecDeque::new(),
            forwards: config.forwards,
        })
    }

    #[handler::tell]
    fn on_register(&mut self, ctx: &mut NativeCtx<'_>, mail: RegisterEngineRouteForTest) {
        self.registration_requests.push_back(mail.engine_id);
        ctx.send::<RpcServerCapability>(&RegisterEngineRoute { engine_id: mail.engine_id });
    }

    #[handler::response]
    fn on_registration_result(&mut self, _ctx: &mut NativeCtx<'_>, mail: crate::RegisterEngineRouteResult) {
        let engine_id = self.registration_requests.pop_front().expect("registration request precedes its result");
        self.registrations.send((engine_id, mail)).expect("registration result receiver stays live");
    }

    // A real handler for the right kind with the wrong contract: silent does
    // not cover EngineRoute's explicit unchecked row.
    #[handler::tell]
    fn on_forward(&mut self, _ctx: &mut NativeCtx<'_>, _mail: crate::ForwardEnvelope) {
        self.forwards.send(()).expect("wrong-route observer stays live");
    }
}

fn request_unchecked_route_registration(
    chassis: &PassiveChassis<TestChassis>,
    route: ActorRef<UncheckedEngineRoute>,
    engine_id: EngineId,
    results: &mpsc::Receiver<(EngineId, crate::RegisterEngineRouteResult)>,
) -> crate::RegisterEngineRouteResult {
    let (_, settled) = chassis.send_tracked(route, &RegisterEngineRouteForTest { engine_id }, None);
    let (answered_engine, result) =
        results.recv_timeout(Duration::from_secs(2)).expect("unchecked route registration answers");
    assert_eq!(answered_engine, engine_id, "registration result is correlated with its request");
    settled.recv_timeout(Duration::from_secs(2)).expect("unchecked route registration chain settles");
    result
}

fn request_wrong_route_registration(
    chassis: &PassiveChassis<TestChassis>,
    route: ActorRef<WrongEngineRoute>,
    engine_id: EngineId,
    results: &mpsc::Receiver<(EngineId, crate::RegisterEngineRouteResult)>,
) -> crate::RegisterEngineRouteResult {
    let (_, settled) = chassis.send_tracked(route, &RegisterEngineRouteForTest { engine_id }, None);
    let (answered_engine, result) =
        results.recv_timeout(Duration::from_secs(2)).expect("wrong route registration answers");
    assert_eq!(answered_engine, engine_id, "registration result is correlated with its request");
    settled.recv_timeout(Duration::from_secs(2)).expect("wrong route registration chain settles");
    result
}

/// Boot a chassis hosting only `RpcServerCapability`, connect a
/// client `TcpStream` to its OS-picked port, and apply
/// `read_timeout`. Tests that need additional caps (e.g.
/// `TestEchoActor`, `TraceDispatchCapability`) build their own
/// chassis and reach for [`connect_to_rpc_server`] for the
/// connect / timeout half. Returns `(chassis, stream)`; both must
/// stay alive for the listener to keep accepting.
fn boot_with_rpc_server_only(timeout: Duration) -> (PassiveChassis<TestChassis>, TcpStream) {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Boot },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("rpc server boots");
    let stream = connect_to_rpc_server(&chassis, timeout);
    (chassis, stream)
}

/// Boot a chassis with the deferred-echo actor + trace dispatch
/// behind the RPC server, connect a client, and complete the
/// handshake. Shared by the deferred-reply settlement tests. Returns
/// `(chassis, stream)`; both must stay alive for the listener.
fn boot_with_deferred_echo(timeout: Duration) -> (PassiveChassis<TestChassis>, TcpStream) {
    use crate::server::test_echo::DeferredEchoActor;

    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<DeferredEchoActor>(())
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Boot },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("caps boot");
    let mut stream = connect_to_rpc_server(&chassis, timeout);
    complete_handshake(&mut stream);
    (chassis, stream)
}

fn boot_with_echo_server() -> PassiveChassis<TestChassis> {
    use crate::server::test_echo::TestEchoActor;

    let (registry, mailer) = fresh_substrate();
    Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<TestEchoActor>(())
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Boot },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("caps boot")
}

/// Lift the published `RpcServerHandle`'s `local_port`, open a
/// `TcpStream`, set `read_timeout`. Shared by every test whose
/// boot path is more elaborate than `boot_with_rpc_server_only`.
fn connect_to_rpc_server(chassis: &PassiveChassis<TestChassis>, timeout: Duration) -> TcpStream {
    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
    let stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to rpc server");
    stream.set_read_timeout(Some(timeout)).expect("test: set_read_timeout on TcpStream");
    stream
}

/// Send a `Hello` carrying the current `WIRE_VERSION` and drain
/// the resulting `HelloAck` so subsequent test traffic sees a
/// clean stream. Tests that want to assert specifically against
/// the handshake reply (handshake_*_roundtrip,
/// `wire_version_mismatch_*`) write the `Hello` themselves so the
/// `HelloAck` / `Bye` can be matched on.
fn complete_handshake(stream: &mut TcpStream) {
    write_frame(
        stream,
        &WireFrame::Hello(Hello {
            wire_version: WIRE_VERSION,
            peer: PeerKind::Client { client_name: "test-client".into(), client_version: "0.0.1".into() },
        }),
    )
    .expect("test: write_frame Hello to rpc server");
    let _: WireFrame = read_frame(stream).expect("test: read_frame after Hello returns HelloAck");
}

fn assert_unknown_engine(stream: &mut TcpStream, engine: EngineId, cid: u64) {
    use crate::{MailEnvelope, RpcError};
    use aether_data::Kind;

    write_frame(
        &mut *stream,
        &WireFrame::Call {
            cid: Some(cid),
            envelope: MailEnvelope {
                to: Recipient {
                    engine: Some(engine),
                    path: ErasedActorPath::new(ABSENT).expect("the absent fixture is a path"),
                },
                kind: <EngineRouteReplyForTest as Kind>::ID,
                payload: EngineRouteReplyForTest { value: 1 }.encode_into_bytes(),
            },
        },
    )
    .expect("write call for an engine without a route");
    assert_eq!(
        read_frame::<_, WireFrame>(stream).expect("engine without a route answers"),
        WireFrame::ReplyEnd { cid, result: Err(RpcError::UnknownEngine { engine }) },
    );
}

/// Boot a `RpcServerCapability` bound to OS-picked port, connect a
/// real TCP client, exchange `Hello` for `HelloAck`. Sanity-check
/// the wire's framing + handshake path end-to-end.
#[test]
fn handshake_hello_to_hello_ack_roundtrip() {
    // Specifically tests the handshake path end-to-end, so it
    // writes the `Hello` itself rather than using
    // `complete_handshake` (which would discard the `HelloAck`
    // before the asserts can inspect it).
    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(2));
    write_frame(
        &mut stream,
        &WireFrame::Hello(Hello {
            wire_version: WIRE_VERSION,
            peer: PeerKind::Client { client_name: "test-client".into(), client_version: "0.0.1".into() },
        }),
    )
    .expect("write Hello");

    let reply: WireFrame = read_frame(&mut stream).expect("read HelloAck");
    match reply {
        WireFrame::HelloAck(HelloAck { wire_version, server }) => {
            assert_eq!(wire_version, WIRE_VERSION);
            match server {
                PeerKind::Substrate { engine_name, .. } => {
                    assert_eq!(engine_name, "test");
                }
                PeerKind::Client { .. } => panic!("expected Substrate peer kind"),
            }
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }
}

/// ADR-0155 §3: a server composed with `bind_addr: None` is disabled —
/// it still claims its `aether.rpc.server` mailbox (so mail to it is
/// diagnosable rather than warn-dropped at an unknown mailbox), but binds
/// no socket and spawns no accept thread, so no `RpcServerHandle` (and
/// hence no listener port) is published.
#[test]
fn disabled_rpc_server_claims_mailbox_and_binds_nothing() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Boot },
            RpcServerConfig { port: None, port_file: None },
        )
        .build_passive()
        .expect("disabled rpc server boots");

    assert!(
        registry.lookup(<RpcServerCapability as Addressable>::NAMESPACE).is_some(),
        "a disabled rpc server still claims its mailbox",
    );
    assert!(
        chassis.handle::<RpcServerHandle>().is_none(),
        "a disabled rpc server binds no socket, so it publishes no handle",
    );
}

/// Issue #6399: a server composed with `RpcBind::Held` and a resolved port
/// refuses every dial and publishes no `RpcServerHandle` until its composer
/// opens the published `RpcBindGate`; after `open` it binds that port and
/// completes a handshake.
#[test]
fn held_rpc_server_accepts_nothing_until_its_gate_opens() {
    use std::io::ErrorKind;
    use std::net::TcpListener;

    let port =
        TcpListener::bind("127.0.0.1:0").and_then(|listener| listener.local_addr()).expect("take a free port").port();
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Held },
            RpcServerConfig { port: Some(port), port_file: None },
        )
        .build_passive()
        .expect("held rpc server boots");

    let refused = TcpStream::connect(("127.0.0.1", port)).expect_err("a held server binds nothing");
    assert_eq!(refused.kind(), ErrorKind::ConnectionRefused);
    assert!(chassis.handle::<RpcServerHandle>().is_none(), "a held server publishes no handle");

    let gate = chassis.handle::<RpcBindGate>().expect("a held server publishes its gate");
    assert_eq!(gate.open().expect("the gate binds its port"), port);

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the opened server");
    stream.set_read_timeout(Some(Duration::from_secs(2))).expect("test: set_read_timeout on TcpStream");
    complete_handshake(&mut stream);
}

/// Issue #6503: a held server composed on port `0` with a port file writes
/// nothing before its gate opens, then reports the port the OS picked — the
/// one `open` returned and a dial completes a handshake on — never the
/// configured `0`.
#[test]
fn held_rpc_server_reports_its_bound_port_when_its_gate_opens() {
    use aether_substrate::testing::{cleanup, scratch_dir};
    use std::fs;

    let dir = scratch_dir("aether-rpc", "port-file");
    let port_file = dir.join("rpc.port");
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Held },
            RpcServerConfig { port: Some(0), port_file: Some(port_file.to_string_lossy().into_owned()) },
        )
        .build_passive()
        .expect("held rpc server boots");

    assert!(!port_file.exists(), "a held server reports no port before its gate opens");

    let port = chassis.handle::<RpcBindGate>().expect("a held server publishes its gate").open().expect("gate binds");
    let reported = fs::read_to_string(&port_file).expect("the opened gate wrote its port file");
    assert_eq!(reported.trim().parse::<u16>().expect("the port file holds a port"), port);

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the reported port");
    stream.set_read_timeout(Some(Duration::from_secs(2))).expect("test: set_read_timeout on TcpStream");
    complete_handshake(&mut stream);
    cleanup(&dir);
}

/// `Ping(token)` round-trips as `Pong(token)`.
#[test]
fn ping_pong_roundtrip() {
    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(2));
    complete_handshake(&mut stream);

    write_frame(&mut stream, &WireFrame::Ping(0x00c0_ffee)).expect("write Ping");
    let reply: WireFrame = read_frame(&mut stream).expect("read Pong");
    assert_eq!(reply, WireFrame::Pong(0x00c0_ffee));
}

/// End-to-end Call dispatch: connect, handshake, fire a `Call`
/// addressed at the test echo actor's `TestEchoRequest` kind,
/// observe a `ReplyEvent { TestEchoReply }` followed by a
/// `ReplyEnd { Ok(()) }` when the chain settles. Exercises the
/// full dispatch / settlement / reply-interception path from
/// phase 2.
#[test]
fn call_echo_round_trip_event_then_end() {
    use crate::MailEnvelope;
    use crate::server::test_echo::{TestEchoActor, TestEchoReply, TestEchoRequest};
    use aether_data::Kind;

    let chassis = boot_with_echo_server();

    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(5));
    complete_handshake(&mut stream);

    // Fire a Call against the echo actor. cid = 0xabc; the cap
    // correlates and ends with ReplyEnd matching the same cid.
    let echo_payload = TestEchoRequest { value: 42 }.encode_into_bytes();
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(0xabc),
            envelope: MailEnvelope {
                to: recipient_of::<TestEchoActor>(),
                kind: <TestEchoRequest as Kind>::ID,
                payload: echo_payload,
            },
        },
    )
    .expect("test: write_frame Call to rpc server");

    // First frame back should be the ReplyEvent carrying the
    // TestEchoReply with the echoed value.
    let event: WireFrame = read_frame(&mut stream).expect("read ReplyEvent");
    let envelope = match event {
        WireFrame::ReplyEvent { cid, envelope } => {
            assert_eq!(cid, 0xabc);
            envelope
        }
        other => panic!("expected ReplyEvent, got {other:?}"),
    };
    assert_eq!(envelope.kind, <TestEchoReply as Kind>::ID);
    let decoded = TestEchoReply::decode_from_bytes(&envelope.payload).expect("decode reply");
    assert_eq!(decoded.value, 42);

    // Then the ReplyEnd closes the call.
    let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
    match end {
        WireFrame::ReplyEnd { cid, result } => {
            assert_eq!(cid, 0xabc);
            result.expect("ReplyEnd result Ok");
        }
        other => panic!("expected ReplyEnd, got {other:?}"),
    }
}

/// A `Call` whose recipient path resolves to no actor in the server's
/// registry is refused on arrival (ADR-0230 section 3): nothing is dispatched
/// and the call closes with `ReplyEnd` `Err(NotPresent)` naming the sent
/// path. Fails if the server dispatches, parks, or closes `Ok` for a path
/// that resolves to nothing (a read timeout or an `Ok` end here), or reports
/// it as some other variant.
#[test]
fn call_to_an_absent_path_closes_not_present() {
    use crate::server::test_echo::TestEchoRequest;
    use crate::{MailEnvelope, RpcError};
    use aether_data::Kind;

    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(5));
    complete_handshake(&mut stream);

    let absent = ErasedActorPath::new(ABSENT).expect("the absent fixture is a path");
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(7),
            envelope: MailEnvelope {
                to: Recipient::local(absent.clone()),
                kind: <TestEchoRequest as Kind>::ID,
                payload: TestEchoRequest { value: 1 }.encode_into_bytes(),
            },
        },
    )
    .expect("test: write_frame Call to rpc server");

    let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
    assert!(
        matches!(&end, WireFrame::ReplyEnd { cid: 7, result: Err(RpcError::NotPresent { path, .. }) } if *path == absent),
        "an absent path closes NotPresent naming it: {end:?}",
    );
}

/// ADR-0233: a wire `Call` carrying engine-only mail closes with `ReplyEnd`
/// `Err(Other)` before any dispatch. The recipient path is absent on purpose:
/// a door that proved the recipient first would answer `NotPresent` instead.
/// Catches a wire client forging a departure notice or a settlement.
#[test]
fn call_carrying_an_engine_only_kind_closes_with_err_before_dispatch() {
    use crate::{MailEnvelope, RpcError};
    use aether_data::Kind;
    use aether_kinds::MonitorNotice;

    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(5));
    complete_handshake(&mut stream);

    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(13),
            envelope: MailEnvelope {
                to: Recipient::local(ErasedActorPath::new(ABSENT).expect("the absent fixture is a path")),
                kind: <MonitorNotice as Kind>::ID,
                payload: MonitorNotice.encode_into_bytes(),
            },
        },
    )
    .expect("test: write_frame Call to rpc server");

    let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
    let reason = format!("{} is engine-only mail", <MonitorNotice as Kind>::ID);
    assert_eq!(end, WireFrame::ReplyEnd { cid: 13, result: Err(RpcError::Other { reason }) });
}

/// A `Call` whose payload the recipient refuses at decode closes with
/// `ReplyEnd` `Err(DecodeRefused)` naming the refuser's path and the kind, and
/// nothing streams before it. Fails if the refusal never reaches the server
/// (an `Ok` end with no reply, as before), if it lands after `Settled`, or if
/// the server streams the notice as a `ReplyEvent` instead of closing on it.
#[test]
fn call_with_a_payload_the_recipient_refuses_closes_decode_refused() {
    use crate::server::test_echo::{TestEchoActor, TestEchoRequest};
    use crate::{MailEnvelope, RpcError};
    use aether_data::Kind;

    let chassis = boot_with_echo_server();
    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(5));
    complete_handshake(&mut stream);

    let recipient = recipient_of::<TestEchoActor>();
    let mut payload = TestEchoRequest { value: 42 }.encode_into_bytes();
    payload.pop();
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(21),
            envelope: MailEnvelope { to: recipient.clone(), kind: <TestEchoRequest as Kind>::ID, payload },
        },
    )
    .expect("test: write_frame Call to rpc server");

    let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
    assert!(
        matches!(
            &end,
            WireFrame::ReplyEnd { cid: 21, result: Err(RpcError::DecodeRefused { path, kind, .. }) }
                if *path == recipient.path && *kind == <TestEchoRequest as Kind>::ID
        ),
        "a refused payload closes DecodeRefused naming the refuser and the kind: {end:?}",
    );
}

/// A `Call` addressed at an engine no proxy has registered closes at once
/// with `ReplyEnd` `Err(UnknownEngine)` naming that engine. Without it the
/// no-route branch returns without writing a `ReplyEnd` and the call hangs
/// (a read timeout here), as an unconfigured forward did before engine
/// routes were registered.
#[test]
fn engine_call_without_a_route_closes_with_unknown_engine() {
    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(5));
    complete_handshake(&mut stream);
    assert_unknown_engine(&mut stream, EngineId(Uuid::from_u128(9)), 11);
}

/// Engine route registration proves the registrant's unchecked forwarding row
/// once, after preserving the existing ownership precedence. A sender with a
/// real but silent `ForwardEnvelope` handler is refused without claiming the
/// engine, so a compatible sender can take that same engine afterward.
#[test]
fn engine_route_registration_is_typed_and_preserves_ownership_precedence() {
    use aether_substrate::Subname;

    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Boot },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("rpc server boots");

    let (alpha_results_tx, alpha_results_rx) = mpsc::channel();
    let (alpha_forwards_tx, _alpha_forwards_rx) = mpsc::channel();
    let alpha = chassis
        .spawn_actor_for_test::<UncheckedEngineRoute>(
            Subname::Named("alpha"),
            UncheckedEngineRouteConfig { registrations: alpha_results_tx, forwards: alpha_forwards_tx },
            (),
        )
        .finish()
        .expect("alpha route spawns");
    let (beta_results_tx, beta_results_rx) = mpsc::channel();
    let (beta_forwards_tx, _beta_forwards_rx) = mpsc::channel();
    let beta = chassis
        .spawn_actor_for_test::<UncheckedEngineRoute>(
            Subname::Named("beta"),
            UncheckedEngineRouteConfig { registrations: beta_results_tx, forwards: beta_forwards_tx },
            (),
        )
        .finish()
        .expect("beta route spawns");
    let (wrong_results_tx, wrong_results_rx) = mpsc::channel();
    let (wrong_forwards_tx, wrong_forwards_rx) = mpsc::channel();
    let wrong = chassis
        .spawn_actor_for_test::<WrongEngineRoute>(
            Subname::Named("wrong"),
            WrongEngineRouteConfig { registrations: wrong_results_tx, forwards: wrong_forwards_tx },
            (),
        )
        .finish()
        .expect("wrong-contract route spawns");

    let alpha_engine = EngineId(Uuid::from_u128(0x0069_4901));
    let beta_engine = EngineId(Uuid::from_u128(0x0069_4902));
    let unused_engine = EngineId(Uuid::from_u128(0x0069_4904));

    assert!(
        matches!(
            request_unchecked_route_registration(&chassis, alpha, alpha_engine, &alpha_results_rx),
            crate::RegisterEngineRouteResult::Ok
        ),
        "an unchecked ForwardEnvelope row is admitted",
    );
    assert!(
        matches!(
            request_unchecked_route_registration(&chassis, alpha, alpha_engine, &alpha_results_rx),
            crate::RegisterEngineRouteResult::Ok
        ),
        "the same owner re-registering its engine stays idempotent",
    );

    let crate::RegisterEngineRouteResult::Err { error } =
        request_wrong_route_registration(&chassis, wrong, alpha_engine, &wrong_results_rx)
    else {
        panic!("the existing engine owner must win before a new registrant's contract is checked");
    };
    assert!(error.contains("already has a registered route"), "engine conflict keeps precedence: {error}");

    let crate::RegisterEngineRouteResult::Err { error } =
        request_unchecked_route_registration(&chassis, alpha, beta_engine, &alpha_results_rx)
    else {
        panic!("one registrant cannot own two engines");
    };
    assert!(error.contains("already routes engine"), "registrant conflict remains distinct: {error}");

    let crate::RegisterEngineRouteResult::Err { error } =
        request_wrong_route_registration(&chassis, wrong, beta_engine, &wrong_results_rx)
    else {
        panic!("a silent ForwardEnvelope handler must not cover EngineRoute's unchecked row");
    };
    assert!(error.contains("EngineRoute"), "wrong-contract refusal names the required protocol: {error}");

    let crate::RegisterEngineRouteResult::Err { error } =
        request_wrong_route_registration(&chassis, wrong, unused_engine, &wrong_results_rx)
    else {
        panic!("a failed cast must not leave an owner row for the incompatible registrant");
    };
    assert!(
        error.contains("EngineRoute") && !error.contains("already routes engine"),
        "a second failed cast sees contract refusal rather than stale ownership: {error}",
    );

    assert!(
        matches!(
            request_unchecked_route_registration(&chassis, beta, beta_engine, &beta_results_rx),
            crate::RegisterEngineRouteResult::Ok
        ),
        "a failed cast changes no route or owner state",
    );

    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(2));
    complete_handshake(&mut stream);
    assert_unknown_engine(&mut stream, unused_engine, 91);
    assert!(
        matches!(wrong_forwards_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "the incompatible actor never receives forwarding after its failed admission",
    );
}

/// A forwarded call is keyed by the typed send's returned id but closes only
/// on the route's remote terminal signal or departure. The fixture deliberately
/// returns from `ForwardEnvelope` without a settlement hold; a settled local
/// forwarding chain must therefore leave the wire call open. Its later reply
/// and `CallSettled` preserve correlation, and departure retires both the route
/// and a pending call.
#[test]
fn forwarded_call_waits_for_remote_terminal_and_route_departure_cleans_up() {
    use crate::{MailEnvelope, RpcError};
    use aether_data::Kind;
    use aether_substrate::Subname;
    use std::io::ErrorKind;

    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Boot },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("rpc server boots");
    let (results_tx, results_rx) = mpsc::channel();
    let (forwards_tx, forwards_rx) = mpsc::channel();
    let route = chassis
        .spawn_actor_for_test::<UncheckedEngineRoute>(
            Subname::Named("remote"),
            UncheckedEngineRouteConfig { registrations: results_tx, forwards: forwards_tx },
            (),
        )
        .finish()
        .expect("unchecked route spawns");
    let engine = EngineId(Uuid::from_u128(0x0069_4903));
    assert!(matches!(
        request_unchecked_route_registration(&chassis, route, engine, &results_rx),
        crate::RegisterEngineRouteResult::Ok
    ));

    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(5));
    complete_handshake(&mut stream);
    let forwarded = |cid, value| WireFrame::Call {
        cid: Some(cid),
        envelope: MailEnvelope {
            to: Recipient {
                engine: Some(engine),
                path: ErasedActorPath::new(ABSENT).expect("the remote path is well formed"),
            },
            kind: <EngineRouteReplyForTest as Kind>::ID,
            payload: EngineRouteReplyForTest { value }.encode_into_bytes(),
        },
    };

    write_frame(&mut stream, &forwarded(41, 7)).expect("write forwarded call");
    forwards_rx.recv_timeout(Duration::from_secs(2)).expect("route receives forwarded call");
    let (_, barrier_settled) = chassis.send_tracked(route, &EngineRouteBarrierForTest, None);
    barrier_settled.recv_timeout(Duration::from_secs(2)).expect("post-forward barrier settles");

    stream.set_read_timeout(Some(Duration::from_millis(150))).expect("set no-data timeout");
    match read_frame::<_, WireFrame>(&mut stream) {
        Err(FrameError::Io(error)) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
        Err(error) => panic!("expected a bounded no-data timeout while the remote call stays open, got {error}"),
        Ok(frame) => panic!("local ForwardEnvelope settlement closed or replied to the remote call: {frame:?}"),
    }
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("restore read timeout");

    let (_, completion_settled) = chassis.send_tracked(route, &CompleteEngineRouteForTest { value: 42 }, None);
    let reply = read_frame::<_, WireFrame>(&mut stream).expect("remote reply reaches the wire caller");
    let envelope = match reply {
        WireFrame::ReplyEvent { cid: 41, envelope } => envelope,
        other => panic!("expected the correlated remote ReplyEvent, got {other:?}"),
    };
    assert_eq!(envelope.kind, <EngineRouteReplyForTest as Kind>::ID);
    assert_eq!(
        EngineRouteReplyForTest::decode_from_bytes(&envelope.payload).expect("decode remote reply"),
        EngineRouteReplyForTest { value: 42 },
    );
    assert_eq!(
        read_frame::<_, WireFrame>(&mut stream).expect("remote terminal closes the wire call"),
        WireFrame::ReplyEnd { cid: 41, result: Ok(()) },
    );
    completion_settled.recv_timeout(Duration::from_secs(2)).expect("completion trigger settles");

    write_frame(&mut stream, &forwarded(42, 8)).expect("write pending forwarded call");
    forwards_rx.recv_timeout(Duration::from_secs(2)).expect("route receives pending call");
    let (_, stopped) = chassis.send_tracked(route, &StopEngineRouteForTest, None);
    let closed = read_frame::<_, WireFrame>(&mut stream).expect("route departure closes its pending call");
    assert!(
        matches!(closed, WireFrame::ReplyEnd { cid: 42, result: Err(RpcError::Other { ref reason }) }
            if reason.contains("left before the call settled")),
        "route departure closes the pending call with its lifecycle reason: {closed:?}",
    );
    stopped.recv_timeout(Duration::from_secs(2)).expect("route shutdown settles");

    write_frame(&mut stream, &forwarded(43, 9)).expect("write call after route departure");
    assert_eq!(
        read_frame::<_, WireFrame>(&mut stream).expect("retired route answers unknown engine"),
        WireFrame::ReplyEnd { cid: 43, result: Err(RpcError::UnknownEngine { engine }) },
    );
}

/// iamacoffeepot/aether#1321 regression: a `Call` routed through the
/// RPC server tags its reply `SourceAddr::Component(rpc_server)`, so
/// a capability that replies via `HubOutbound::send_reply` (which
/// only routes `Session` / `EngineMailbox`) drops the reply silently —
/// the same drop #1316/#1319 fixed for the desktop driver. The
/// `WindowCapability`, on its synthetic backend, `Ok`-replies on `list`;
/// with the bug present this `Call` would yield a bare `ReplyEnd` and zero
/// `ReplyEvent`s. Routing through the `Mailer` (the complete router)
/// pushes the reply back locally to the server's `on_any`, so the
/// reply rides home as a `ReplyEvent` before the `ReplyEnd`.
#[test]
fn call_window_list_reaches_component_reply() {
    use crate::MailEnvelope;
    use aether_data::Kind;
    use aether_window::{ListWindows, ListWindowsResult, WindowCapability, WindowParams};

    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<WindowCapability>(WindowParams::Synthetic)
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Boot },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("caps boot");

    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(5));
    complete_handshake(&mut stream);

    let payload = ListWindows.encode_into_bytes();
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(0xdef),
            envelope: MailEnvelope { to: recipient_of::<WindowCapability>(), kind: <ListWindows as Kind>::ID, payload },
        },
    )
    .expect("test: write_frame Call to rpc server");

    // The reply must arrive as a ReplyEvent — the drop this
    // test guards against would leave zero events before ReplyEnd.
    let event: WireFrame = read_frame(&mut stream).expect("read ReplyEvent");
    let envelope = match event {
        WireFrame::ReplyEvent { cid, envelope } => {
            assert_eq!(cid, 0xdef);
            envelope
        }
        other => panic!("expected ReplyEvent, got {other:?}"),
    };
    assert_eq!(envelope.kind, <ListWindowsResult as Kind>::ID);
    let decoded = ListWindowsResult::decode_from_bytes(&envelope.payload).expect("decode ListWindowsResult");
    assert!(
        matches!(&decoded, ListWindowsResult::Ok { windows } if windows.is_empty()),
        "the window manager lists its (empty) window set, got {decoded:?}",
    );

    let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
    match end {
        WireFrame::ReplyEnd { cid, result } => {
            assert_eq!(cid, 0xdef);
            result.expect("ReplyEnd result Ok");
        }
        other => panic!("expected ReplyEnd, got {other:?}"),
    }
}

/// iamacoffeepot/aether#1031 end-to-end: a `Call` against an actor
/// that replies through the ADR-0093 hold-until-resolve dispatch
/// (spawned worker -> completion wake -> re-reply) must still
/// produce a `ReplyEvent` followed by a `ReplyEnd`. The settlement
/// hold keeps the chain open across the spawn, so the RPC server's
/// settlement subscription wakes only *after* the deferred reply
/// arrives — not when the handler returns. Pre-fix the chain settled
/// the instant `on_deferred_echo` returned and the deferred reply
/// landed in an already-closed call (no `ReplyEvent`, only a bare
/// `ReplyEnd`, then the late reply dropped).
#[test]
fn call_deferred_echo_settles_after_reply() {
    use crate::MailEnvelope;
    use crate::server::test_echo::{DeferredEchoActor, DeferredEchoReply, DeferredEchoRequest};
    use aether_data::Kind;

    let (_chassis, mut stream) = boot_with_deferred_echo(Duration::from_secs(5));

    let payload = DeferredEchoRequest { value: 99 }.encode_into_bytes();
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(0xdef),
            envelope: MailEnvelope {
                to: recipient_of::<DeferredEchoActor>(),
                kind: <DeferredEchoRequest as Kind>::ID,
                payload,
            },
        },
    )
    .expect("test: write_frame Call to rpc server");

    // The deferred reply arrives as a ReplyEvent — proving the chain
    // stayed open long enough for the spawned worker's reply to be
    // intercepted (not dropped into an already-settled call).
    let event: WireFrame = read_frame(&mut stream).expect("read ReplyEvent");
    let envelope = match event {
        WireFrame::ReplyEvent { cid, envelope } => {
            assert_eq!(cid, 0xdef);
            envelope
        }
        other => panic!("expected ReplyEvent for the deferred reply, got {other:?}"),
    };
    assert_eq!(envelope.kind, <DeferredEchoReply as Kind>::ID);
    let decoded = DeferredEchoReply::decode_from_bytes(&envelope.payload).expect("decode deferred reply");
    assert_eq!(decoded.value, 99);

    // ReplyEnd follows — settlement fired after the deferred reply,
    // not when the handler returned.
    let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
    match end {
        WireFrame::ReplyEnd { cid, result } => {
            assert_eq!(cid, 0xdef);
            result.expect("ReplyEnd result Ok");
        }
        other => panic!("expected ReplyEnd, got {other:?}"),
    }
}

/// A `Call` carrying a `DispatchTraced` batch with **two**
/// `DeferredEchoRequest` envelopes — the empirical `send_mail_traced`
/// failure shape: each child is itself a deferred-reply path
/// (spawn → loopback → re-reply), routed through the trace cap rather
/// than directly. Pre-fix the trace cap dispatched each child via
/// `ctx.send_envelope_tracked` which stamps `reply_to` at the
/// dispatcher's own mailbox (the `push_envelope_buffered` default);
/// child deferred replies landed at the trace cap, which has no
/// handler for the reply kind and no `#[fallback]`, so they were
/// silently dropped. The wire call closed via the (still correct)
/// settlement signal with `replies: []`. The fix forwards each
/// child's `reply_to` to the trace cap's own inbound `reply_target`
/// (typically the RPC server holding the wire `cid`'s in-flight
/// entry), so child replies — sync or deferred — bubble through to
/// the wire as `ReplyEvent`s, and settlement still fires only after
/// each hold-until-resolve dispatch's hold drops.
///
/// Test asserts: TWO `ReplyEvent`s (one `DeferredEchoReply` per
/// request), then exactly ONE `ReplyEnd`. Order of the two events is
/// unspecified (the two deferred-echo handlers run in parallel
/// behind 50ms sleeps); the test pairs by `value`.
#[test]
fn dispatch_traced_with_deferred_replies_routes_each_event_then_settles() {
    use crate::MailEnvelope;
    use crate::server::test_echo::{DeferredEchoActor, DeferredEchoReply, DeferredEchoRequest};
    use aether_data::Kind;
    use aether_kinds::NamedMail;
    use aether_kinds::trace::DispatchTraced;
    use aether_trace::TraceDispatchCapability;

    let (_chassis, mut stream) = boot_with_deferred_echo(Duration::from_secs(10));

    // Build a batched DispatchTraced with two DeferredEchoRequest
    // envelopes, addressed at the deferred-echo actor by name (the
    // trace cap resolves names through the registry).
    let recipient = || {
        ErasedActorPath::new(<DeferredEchoActor as Addressable>::NAMESPACE)
            .expect("a namespace is a well-formed actor path")
    };
    let batch = DispatchTraced {
        mails: vec![
            NamedMail {
                recipient: recipient(),
                kind_name: <DeferredEchoRequest as Kind>::NAME.into(),
                payload: DeferredEchoRequest { value: 11 }.encode_into_bytes(),
                count: 1,
            },
            NamedMail {
                recipient: recipient(),
                kind_name: <DeferredEchoRequest as Kind>::NAME.into(),
                payload: DeferredEchoRequest { value: 22 }.encode_into_bytes(),
                count: 1,
            },
        ],
    };
    let payload = batch.encode_into_bytes();
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(0xbeef),
            envelope: MailEnvelope {
                to: recipient_of::<TraceDispatchCapability>(),
                kind: <DispatchTraced as Kind>::ID,
                payload,
            },
        },
    )
    .expect("test: write_frame Call DispatchTraced to rpc server");

    // The trace cap's synchronous `DispatchTracedAck::Ok` reply
    // arrives as a ReplyEvent. Drain it before scanning for the two
    // deferred replies — its ordering is well-defined (the trace
    // handler replies before the children run), so we can read it
    // first without an unbound search.
    let mut deferred_values: Vec<u64> = Vec::new();
    let mut saw_ack = false;
    // Drain up to 4 ReplyEvent frames (ack + 2 deferred + safety
    // margin) before the ReplyEnd. Each iteration consumes one
    // frame; the ReplyEnd breaks.
    loop {
        let frame: WireFrame = read_frame(&mut stream).expect("read frame");
        match frame {
            WireFrame::ReplyEvent { cid, envelope } => {
                assert_eq!(cid, 0xbeef);
                if envelope.kind == <DeferredEchoReply as Kind>::ID {
                    let decoded =
                        DeferredEchoReply::decode_from_bytes(&envelope.payload).expect("decode deferred reply");
                    deferred_values.push(decoded.value);
                } else {
                    // Otherwise this is the DispatchTracedAck::Ok
                    // reply; mark it observed but don't assert on
                    // its payload here (the ack carries the root
                    // MailId; the test's load-bearing assertions are
                    // on the deferred-reply payloads).
                    saw_ack = true;
                }
            }
            WireFrame::ReplyEnd { cid, result } => {
                assert_eq!(cid, 0xbeef);
                result.expect("ReplyEnd result Ok");
                break;
            }
            other => panic!("expected ReplyEvent / ReplyEnd, got {other:?}"),
        }
    }
    assert!(saw_ack, "expected DispatchTracedAck reply event before ReplyEnd");
    deferred_values.sort_unstable();
    assert_eq!(deferred_values, vec![11, 22], "expected one DeferredEchoReply per request, sorted by value");
}

/// Fire-and-forget `Call { cid: None }` skips reply correlation
/// entirely — no settlement subscription is created, no
/// `ReplyEnd` is written. Verify by sending a Call with cid None
/// at the test echo actor (whose reply would otherwise come back
/// as a `ReplyEvent` if correlation had leaked) and confirming a
/// subsequent `Ping(token)` round-trips immediately, which proves
/// no stale `ReplyEvent` / `ReplyEnd` frames are in the way.
#[test]
fn call_without_cid_is_fire_and_forget() {
    use crate::MailEnvelope;
    use crate::server::test_echo::{TestEchoActor, TestEchoRequest};
    use aether_data::Kind;

    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TestEchoActor>(())
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Boot },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("caps boot");

    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(2));
    complete_handshake(&mut stream);

    // Fire-and-forget Call (cid = None). The echo actor will
    // still reply, but with cid None there's no in-flight entry
    // so the reply has no matching correlation and gets dropped.
    let echo_payload = TestEchoRequest { value: 7 }.encode_into_bytes();
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: None,
            envelope: MailEnvelope {
                to: recipient_of::<TestEchoActor>(),
                kind: <TestEchoRequest as Kind>::ID,
                payload: echo_payload,
            },
        },
    )
    .expect("test: write_frame fire-and-forget Call to rpc server");

    // Immediately Ping. If the fire-and-forget Call had leaked
    // reply correlation, a ReplyEvent / ReplyEnd would arrive
    // before the Pong. Asserting we see Pong first proves no leak.
    write_frame(&mut stream, &WireFrame::Ping(0x00c0_ffee)).expect("test: write_frame Ping to rpc server");
    let reply: WireFrame = read_frame(&mut stream).expect("read Pong");
    assert_eq!(reply, WireFrame::Pong(0x00c0_ffee));
}

/// A `Hello` carrying a mismatched `wire_version` triggers a `Bye`
/// and connection close on the server side.
#[test]
fn wire_version_mismatch_kicks_connection() {
    // Sends a deliberately wrong `wire_version` and asserts the
    // server responds with `Bye`, so it can't use
    // `complete_handshake` (which sends the current version).
    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(2));
    write_frame(
        &mut stream,
        &WireFrame::Hello(Hello {
            wire_version: WIRE_VERSION + 1,
            peer: PeerKind::Client { client_name: "future-client".into(), client_version: "9.9.9".into() },
        }),
    )
    .expect("test: write_frame future-version Hello to rpc server");

    let reply: WireFrame = read_frame(&mut stream).expect("read Bye");
    match reply {
        WireFrame::Bye { reason } => {
            assert!(reason.contains("wire_version"), "Bye reason should mention wire_version: {reason}");
        }
        other => panic!("expected Bye, got {other:?}"),
    }
}

/// iamacoffeepot/aether#1271: an inbound frame whose announced
/// length exceeds the framing cap but is within the drain ceiling
/// (`size <= 2 * max`) is fail-soft. The server drains the body,
/// writes a `ReplyEnd { cid: 0, Err(RpcError::FrameTooLarge) }`,
/// and keeps the connection alive — a follow-up `Ping` round-trips
/// as `Pong`, proving the session survived.
#[test]
fn oversize_frame_replies_with_frame_too_large_and_session_survives() {
    use crate::RpcError;
    use aether_codec::frame::{MAX_FRAME_SIZE, max_frame_size};
    use std::io::Write;

    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(5));
    complete_handshake(&mut stream);

    // Set the read timeout high — the server has to read the full
    // oversize body off the wire before it can write the error
    // reply, so the read for the ReplyEnd is gated on that drain.
    stream.set_write_timeout(Some(Duration::from_secs(10))).expect("set_write_timeout");

    // Announce a body just over the cap, then push that many zero
    // bytes. The cap defaults to 64 MiB (MAX_FRAME_SIZE), and the
    // process-wide accessor caches on first read — so the drain
    // ceiling is exactly `2 * max_frame_size()`. Pick the smallest
    // legal oversize: max + 1.
    let max = max_frame_size();
    assert!(max >= MAX_FRAME_SIZE, "cap accessor lifted below default");
    let oversize: usize = max + 1;
    assert!(oversize <= max.saturating_mul(2), "test size must be inside the drain ceiling");
    #[allow(clippy::cast_possible_truncation)]
    let prefix = (oversize as u32).to_le_bytes();
    stream.write_all(&prefix).expect("write oversize length prefix");
    // Write the body in chunks so a 64 MiB+ payload doesn't single-
    // syscall through.
    let chunk = vec![0u8; 1024 * 1024];
    let mut remaining = oversize;
    while remaining > 0 {
        let n = remaining.min(chunk.len());
        stream.write_all(&chunk[..n]).expect("write oversize body chunk");
        remaining -= n;
    }

    // The server replies with a structured ReplyEnd carrying
    // FrameTooLarge. cid is 0 (the sentinel for "wire-level error,
    // no in-flight cid to bind to").
    let reply: WireFrame = read_frame(&mut stream).expect("read fail-soft ReplyEnd");
    match reply {
        WireFrame::ReplyEnd { cid, result } => {
            assert_eq!(cid, 0, "fail-soft uses cid=0 sentinel");
            match result {
                Err(RpcError::FrameTooLarge { size, max: cap }) => {
                    assert_eq!(size, oversize as u64);
                    assert_eq!(cap, max as u64);
                }
                other => panic!("expected FrameTooLarge, got {other:?}"),
            }
        }
        other => panic!("expected ReplyEnd, got {other:?}"),
    }

    // Ping/Pong round-trips — the session is still alive.
    write_frame(&mut stream, &WireFrame::Ping(0xfeed_face)).expect("write Ping after fail-soft");
    let pong: WireFrame = read_frame(&mut stream).expect("read Pong after fail-soft");
    assert_eq!(pong, WireFrame::Pong(0xfeed_face));
}

fn client_peer_kind() -> PeerKind {
    PeerKind::Client { client_name: "rpc-client-test".into(), client_version: "0.0.1".into() }
}

/// Full socket round-trip: boot `RpcServerCapability` + the echo
/// actor + `TraceDispatchCapability`, connect a real
/// [`RpcClient`](crate::wire::RpcClient), fire a `Call` carrying a
/// `TestEchoRequest`, and drain the inbound channel — expect
/// `ReplyEvent { TestEchoReply }` then `ReplyEnd { Ok }`. This is the
/// only test exercising the actual TCP client↔server path end to end
/// (the `RpcClient` half lives in the sibling `wire` module per
/// ADR-0124; this integration test stays here, where the server lives).
#[test]
fn call_echo_round_trips_over_the_socket() {
    use crate::server::test_echo::{TestEchoActor, TestEchoReply, TestEchoRequest};
    use crate::{MailEnvelope, RpcClient};
    use aether_data::Kind;

    let chassis = boot_with_echo_server();

    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;

    // No on_frame work needed — `recv_timeout` returning is the
    // observable signal we care about. iamacoffeepot/aether#835:
    // a prior version asserted `frames_seen >= 2` against an
    // AtomicUsize bumped inside the hook, but the hook is a
    // post-enqueue scheduling kick by design — the test thread can
    // wake from `recv_timeout` before the reader thread reaches
    // `on_frame()`, racing the assertion. End-to-end correctness
    // here is the two `recv_timeout` returns below: ReplyEvent then
    // ReplyEnd.
    let mut conn = RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {})
        .expect("client connects + handshakes");

    // The handshake handed back the server's identity.
    match &conn.server {
        PeerKind::Substrate { engine_name, .. } => assert_eq!(engine_name, "test"),
        PeerKind::Client { .. } => panic!("expected Substrate peer kind from server"),
    }

    let echo_payload = TestEchoRequest { value: 42 }.encode_into_bytes();
    let cid = conn
        .client
        .call(MailEnvelope {
            to: recipient_of::<TestEchoActor>(),
            kind: <TestEchoRequest as Kind>::ID,
            payload: echo_payload,
        })
        .expect("call writes");

    // First frame back: ReplyEvent carrying the echoed reply.
    // recv_timeout so a hung settlement fails the test instead of
    // blocking forever.
    let event = conn.inbound.recv_timeout(Duration::from_secs(5)).expect("ReplyEvent within 5s");
    let envelope = match event {
        WireFrame::ReplyEvent { cid: ev_cid, envelope } => {
            assert_eq!(ev_cid, cid);
            envelope
        }
        other => panic!("expected ReplyEvent, got {other:?}"),
    };
    assert_eq!(envelope.kind, <TestEchoReply as Kind>::ID);
    let decoded = TestEchoReply::decode_from_bytes(&envelope.payload).expect("decode reply");
    assert_eq!(decoded.value, 42);

    // Then ReplyEnd closes the call.
    let end = conn.inbound.recv_timeout(Duration::from_secs(5)).expect("ReplyEnd within 5s");
    match end {
        WireFrame::ReplyEnd { cid: end_cid, result } => {
            assert_eq!(end_cid, cid);
            result.expect("ReplyEnd result Ok");
        }
        other => panic!("expected ReplyEnd, got {other:?}"),
    }
}

/// Asks [`BlobSharer`] for a reply whose blob is `blob_len` patterned
/// bytes and whose `padding` is `padding_len` bytes of text.
#[aether_data::kind(name = "aether.rpc.test.blob", copy, default, eq)]
struct BlobRequest {
    blob_len: u64,
    padding_len: u64,
}

/// [`BlobSharer`]'s reply: one blob plus padding that sizes the payload.
#[aether_data::kind(name = "aether.rpc.test.blob_result")]
struct BlobResult {
    blob: aether_data::Blob,
    padding: String,
}

/// Replies to each [`BlobRequest`] with its blob checked into the engine
/// store. The reply to the rpc server's component mailbox is in-process, so
/// it carries the blob as tag 1 with the entry attached, and the rpc server
/// must write it out as bytes.
struct BlobSharer {
    /// The character each reply's padding repeats.
    padding_fill: char,
}

#[aether_actor::actor(singleton, root)]
impl NativeActor for BlobSharer {
    type Config = ();
    const NAMESPACE: &'static str = "aether.rpc.test.blob_sharer";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { padding_fill: 'p' })
    }

    /// Reply with a shared blob.
    #[handler::request]
    fn on_blob_request(&mut self, ctx: &mut NativeCtx<'_, Self>, mail: BlobRequest) -> BlobResult {
        let blob = ctx.check_in(patterned(mail.blob_len).into_boxed_slice());
        let padding =
            self.padding_fill.to_string().repeat(usize::try_from(mail.padding_len).expect("test padding fits memory"));
        BlobResult { blob, padding }
    }
}

fn patterned(len: u64) -> Vec<u8> {
    (0..len).map(|i| u8::try_from(i % 251).expect("below 251")).collect()
}

fn boot_with_blob_replier() -> (PassiveChassis<TestChassis>, TcpStream) {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<BlobSharer>(())
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Boot },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("caps boot");
    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(10));
    complete_handshake(&mut stream);
    (chassis, stream)
}

fn call_blob_replier(stream: &mut TcpStream, cid: u64, request: BlobRequest) {
    use crate::MailEnvelope;
    use aether_data::Kind;

    write_frame(
        stream,
        &WireFrame::Call {
            cid: Some(cid),
            envelope: MailEnvelope {
                to: recipient_of::<BlobSharer>(),
                kind: <BlobRequest as Kind>::ID,
                payload: request.encode_into_bytes(),
            },
        },
    )
    .expect("write Call");
}

/// Read one call's `ReplyEvent` and `ReplyEnd { Ok }`, returning the reply's
/// blob bytes. The plain decode refuses a tag-1 field, so a payload that left
/// the process unrewritten fails here.
fn read_blob_reply(stream: &mut TcpStream, cid: u64) -> Vec<u8> {
    use aether_data::{BlobReader, Kind};

    let envelope = match read_frame(stream).expect("read ReplyEvent") {
        WireFrame::ReplyEvent { cid: event_cid, envelope } if event_cid == cid => envelope,
        other => panic!("expected ReplyEvent for cid {cid}, got {other:?}"),
    };
    assert_eq!(envelope.kind, <BlobResult as Kind>::ID);
    let reply = BlobResult::decode_from_bytes(&envelope.payload).expect("the reply decodes with inline blob bytes");
    let reader = BlobReader::open(&reply.blob);
    let mut bytes = vec![0; usize::try_from(reader.len()).expect("test blob fits memory")];
    let mut filled = 0;
    while filled < bytes.len() {
        filled += reader.read_range(filled as u64, &mut bytes[filled..]);
    }

    match read_frame(stream).expect("read ReplyEnd") {
        WireFrame::ReplyEnd { cid: end_cid, result } if end_cid == cid => result.expect("ReplyEnd Ok"),
        other => panic!("expected ReplyEnd for cid {cid}, got {other:?}"),
    }
    bytes
}

/// A reply carrying an attached blob reaches the wire client as tag-0 bytes
/// holding the blob's contents. Catches a tag-1 hash written into a
/// `ReplyEvent`, which no client can resolve.
#[test]
fn attached_reply_reaches_the_client_as_inline_bytes() {
    let (_chassis, mut stream) = boot_with_blob_replier();

    call_blob_replier(&mut stream, 0x0b10, BlobRequest { blob_len: 300, padding_len: 0 });

    assert_eq!(read_blob_reply(&mut stream, 0x0b10), patterned(300));
}

/// A reply whose inline form cannot fit one frame closes its own call with
/// `FrameTooLarge` naming the size and the limit, and the connection carries
/// the next call. Catches the reply-out writing the oversized frame, which
/// closes the whole connection, or leaving the call open.
#[test]
fn oversized_attached_reply_closes_the_call_and_keeps_the_connection() {
    use crate::RpcError;
    use aether_codec::frame::max_frame_size;

    let (_chassis, mut stream) = boot_with_blob_replier();
    let max = max_frame_size();

    call_blob_replier(&mut stream, 0x0b11, BlobRequest { blob_len: 16, padding_len: max as u64 });
    match read_frame(&mut stream).expect("read ReplyEnd") {
        WireFrame::ReplyEnd { cid: 0x0b11, result: Err(RpcError::FrameTooLarge { size, max: limit }) } => {
            assert!(size > max as u64, "the reported size {size} must exceed the limit {max}");
            assert_eq!(limit, max as u64);
        }
        other => panic!("expected ReplyEnd FrameTooLarge for cid 0x0b11, got {other:?}"),
    }

    call_blob_replier(&mut stream, 0x0b12, BlobRequest { blob_len: 16, padding_len: 0 });
    assert_eq!(read_blob_reply(&mut stream, 0x0b12), patterned(16));
}

/// Asks [`LargeReplier`] for a blob-free reply whose body is `body_len`
/// bytes of text.
#[aether_data::kind(name = "aether.rpc.test.large_request", copy, default, eq)]
struct LargeRequest {
    body_len: u64,
}

/// [`LargeReplier`]'s reply: padding that sizes the payload with no blob
/// attached.
#[aether_data::kind(name = "aether.rpc.test.large_result")]
struct LargeResult {
    body: String,
}

/// Replies to each [`LargeRequest`] with a plain string body. The reply
/// carries no blob, so it passes `wire_payload` unwalked and its first
/// size check is the `ReplyEvent` frame encode.
struct LargeReplier;

#[aether_actor::actor(singleton, root)]
impl NativeActor for LargeReplier {
    type Config = ();
    const NAMESPACE: &'static str = "aether.rpc.test.large_replier";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    /// Reply with a plain body sized by the request.
    #[handler::request]
    fn on_large_request(&mut self, _ctx: &mut NativeCtx<'_>, mail: LargeRequest) -> LargeResult {
        let len = usize::try_from(mail.body_len).expect("test body fits memory");
        LargeResult { body: "l".repeat(len) }
    }
}

fn boot_with_large_replier() -> (PassiveChassis<TestChassis>, TcpStream) {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<LargeReplier>(())
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams { peer_kind: test_peer_kind(), bind: RpcBind::Boot },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("caps boot");
    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(10));
    complete_handshake(&mut stream);
    (chassis, stream)
}

fn call_large_replier(stream: &mut TcpStream, cid: u64, request: LargeRequest) {
    use crate::MailEnvelope;
    use aether_data::Kind;

    write_frame(
        stream,
        &WireFrame::Call {
            cid: Some(cid),
            envelope: MailEnvelope {
                to: recipient_of::<LargeReplier>(),
                kind: <LargeRequest as Kind>::ID,
                payload: request.encode_into_bytes(),
            },
        },
    )
    .expect("write Call");
}

/// Read one call's `ReplyEvent` and `ReplyEnd { Ok }`, returning the reply
/// body.
fn read_large_reply(stream: &mut TcpStream, cid: u64) -> String {
    use aether_data::Kind;

    let envelope = match read_frame(stream).expect("read ReplyEvent") {
        WireFrame::ReplyEvent { cid: event_cid, envelope } if event_cid == cid => envelope,
        other => panic!("expected ReplyEvent for cid {cid}, got {other:?}"),
    };
    assert_eq!(envelope.kind, <LargeResult as Kind>::ID);
    let reply = LargeResult::decode_from_bytes(&envelope.payload).expect("the reply decodes");

    match read_frame(stream).expect("read ReplyEnd") {
        WireFrame::ReplyEnd { cid: end_cid, result } if end_cid == cid => result.expect("ReplyEnd Ok"),
        other => panic!("expected ReplyEnd for cid {cid}, got {other:?}"),
    }
    reply.body
}

/// A blob-free reply that cannot fit one frame closes its own call with
/// `FrameTooLarge` naming the size and the cap, and the connection carries
/// the next call. Catches the reply-event write closing the whole
/// connection on `EncodeTooLarge`, or leaving the failed call open.
#[test]
fn oversized_plain_reply_closes_the_call_and_keeps_the_connection() {
    use crate::RpcError;
    use aether_codec::frame::max_frame_size;

    let (_chassis, mut stream) = boot_with_large_replier();
    let max = max_frame_size();

    call_large_replier(&mut stream, 0x0c11, LargeRequest { body_len: max as u64 });
    match read_frame(&mut stream).expect("read ReplyEnd") {
        WireFrame::ReplyEnd { cid: 0x0c11, result: Err(RpcError::FrameTooLarge { size, max: limit }) } => {
            assert!(size > max as u64, "the reported size {size} must exceed the limit {max}");
            assert_eq!(limit, max as u64);
        }
        other => panic!("expected ReplyEnd FrameTooLarge for cid 0x0c11, got {other:?}"),
    }

    call_large_replier(&mut stream, 0x0c12, LargeRequest { body_len: 16 });
    assert_eq!(read_large_reply(&mut stream, 0x0c12), "l".repeat(16));
}
