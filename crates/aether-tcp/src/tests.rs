//! Tests for the `aether.tcp` control plane: connect / bind / list / unbind
//! round-trips through a passive chassis with a real loopback socket.
#![allow(clippy::disallowed_methods, reason = "these tests boot a bare `TestChassis` through `Builder::new`")] // aether-suppression-request: pre-existing file-level allow whose reason is rewritten; the tests still build a bare chassis with the disallowed `Builder::new`

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::{
    BindListener, BindListenerError, BindListenerResult, BindListenerSelf, Connect, ConnectError, ConnectResult,
    ListListeners, ListListenersResult, SessionClosed, SessionData, SessionWrite, TcpCapability, TcpConsumer,
    TcpListenerActor, TcpSessionActor, UnbindListener, UnbindListenerResult,
};
use aether_actor::{
    ActorPath, Addressable, ErasedActorRef, PathRefusal, PathRefused, ProtocolPath, ProtocolRef, Unchecked, Undeclared,
    actor,
};
use aether_data::{ErasedActorPath, Kind, LoadName, SessionToken, Uuid};
use aether_kinds::descriptors;
use aether_substrate::actor::native::spawn::Subname;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnOutcome, TaskDone};
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::SettlementConfig;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
use aether_substrate::mail::registry::{OwnedDispatch, Registry};
use aether_substrate::testing::{
    PumpedDriver, TestChassis, await_settled, boot_authority, boot_bare_test_chassis, registered_ref, withdraw_ref,
};
use aether_substrate::{ChassisTarget, ReplyTarget};

mod consumer_close;

fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>, mpsc::Receiver<EgressEvent>) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(&boot_authority(), d);
    }
    let (outbound, rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    (registry, mailer, rx)
}

/// Boot a fresh substrate with `TcpCapability` registered as a
/// passive actor and return the pieces every test in this
/// module reaches for: the kind registry (for route collisions and
/// address resolution), the egress receiver (for reply
/// decode), and the [`PassiveChassis`] (held by the caller so
/// the cap's actor thread stays alive for the test body).
///
/// Collapses the previously-duplicated `fresh_substrate()` +
/// `Builder::<TestChassis>::new(...)` chain that opened every
/// test (issue 796).
fn boot_tcp_substrate() -> (Arc<Registry>, Arc<Mailer>, mpsc::Receiver<EgressEvent>, PassiveChassis<TestChassis>) {
    boot_tcp_substrate_with(|builder| builder)
}

/// [`boot_tcp_substrate`] with further actors composed beside
/// `TcpCapability` by `compose`.
fn boot_tcp_substrate_with(
    compose: impl FnOnce(Builder<TestChassis>) -> Builder<TestChassis>,
) -> (Arc<Registry>, Arc<Mailer>, mpsc::Receiver<EgressEvent>, PassiveChassis<TestChassis>) {
    let (registry, mailer, rx) = fresh_substrate();
    let chassis = compose(
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer)).with_actor::<TcpCapability>(()),
    )
    .build_passive()
    .expect("TcpCapability boots");
    (registry, mailer, rx, chassis)
}

fn session_reply() -> ReplyTarget {
    ReplyTarget::Session { session: SessionToken(Uuid::from_u128(0xfeed)), correlation: 0 }
}

/// Push `mail` to `to` as a tracked chassis root and wait for its whole
/// chain to settle, so every reply the chain sends is already on egress.
fn send_and_settle<K: Kind, I>(
    chassis: &PassiveChassis<TestChassis>,
    to: impl ChassisTarget<K, I>,
    mail: &K,
    reply: Option<ReplyTarget>,
) {
    let (_, settled) = chassis.send_tracked(to, mail, reply);
    await_settled(&settled, K::NAME);
}

/// Take the next egress event, which a settled chain has already sent, as a
/// session reply of kind `R`: its session, its correlation, and the reply.
fn next_reply<R: Kind>(rx: &mpsc::Receiver<EgressEvent>, what: &str) -> (SessionToken, u64, R) {
    match rx.try_recv() {
        Ok(EgressEvent::ToSession { session, kind_name, payload, correlation_id, .. }) => {
            assert_eq!(kind_name, R::NAME, "{what} is not a {}", R::NAME);
            (session, correlation_id, R::decode_from_bytes(&payload).expect("decode reply"))
        }
        other => panic!("expected {what} as a session reply, got {other:?}"),
    }
}

#[derive(Debug)]
enum CapturedSessionMail {
    Data(SessionData),
    Closed(SessionClosed),
}

/// A session consumer: it covers [`TcpConsumer`] with silent handlers, so a
/// `ProtocolPath<TcpConsumer>` to it decodes, and forwards each delivery to
/// the test over its config's channel. A kind outside the protocol has no
/// handler and is warn-dropped, and a send whose receiver has already dropped
/// is discarded: a session still live when the test body ends mails its
/// `SessionClosed` on peer EOF after the receiver is gone, and losing that
/// late capture is correct, since a test that wants it awaits it.
///
/// It stands at a root instance ([`spawn_consumer`]) or, beneath
/// [`ConsumerHost`], at a nested lineage position: the shape a loaded wasm
/// component has.
struct SessionConsumer {
    captures: mpsc::Sender<CapturedSessionMail>,
}

#[actor(instanced, root, child_of(ConsumerHost))]
impl NativeActor for SessionConsumer {
    const NAMESPACE: &'static str = "test.tcp.consumer";
    type Config = mpsc::Sender<CapturedSessionMail>;

    fn init(captures: mpsc::Sender<CapturedSessionMail>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { captures })
    }

    #[handler::tell]
    fn on_session_data(&mut self, _ctx: &mut NativeCtx<'_>, mail: SessionData) {
        let _ = self.captures.send(CapturedSessionMail::Data(mail));
    }

    #[handler::tell]
    fn on_session_closed(&mut self, _ctx: &mut NativeCtx<'_>, mail: SessionClosed) {
        let _ = self.captures.send(CapturedSessionMail::Closed(mail));
    }

    #[handler::tell]
    fn on_shut_down(&mut self, ctx: &mut NativeCtx<'_>, _mail: ShutDown) {
        ctx.shutdown();
    }
}

/// Tells a [`SessionConsumer`] to shut itself down, the way a consumer that
/// is done, or is torn down, closes without unbinding anything.
#[aether_data::kind(name = "test.tcp.shut_down", copy)]
struct ShutDown;

/// The key [`ConsumerHost`] spawns its nested [`SessionConsumer`] under.
const NESTED_CONSUMER_KEY: &str = "probe";

/// A root singleton whose `wire` stages one [`SessionConsumer`] beneath
/// itself at [`NESTED_CONSUMER_KEY`], handing it the capture channel, and
/// signals its birth channel once the registry owner has decided the birth.
struct ConsumerHost {
    captures: Option<mpsc::Sender<CapturedSessionMail>>,
    born: mpsc::Sender<()>,
    staged: Option<ErasedActorPath>,
}

#[actor(singleton, root)]
impl NativeActor for ConsumerHost {
    const NAMESPACE: &'static str = "test.tcp.consumer_host";
    type Config = ();
    type Params = (mpsc::Sender<CapturedSessionMail>, mpsc::Sender<()>);

    fn init(
        (): (),
        (captures, born): (mpsc::Sender<CapturedSessionMail>, mpsc::Sender<()>),
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<Self, BootError> {
        Ok(Self { captures: Some(captures), born, staged: None })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        let captures = self.captures.take().expect("wire runs once");
        let receipt = ctx
            .spawn_child::<SessionConsumer>(Subname::Named(NESTED_CONSUMER_KEY), captures, ())
            .stage()
            .expect("the nested consumer stages");
        self.staged = Some(receipt.canonical_name);
        Ok(())
    }

    #[handler(task)]
    fn on_consumer_born(&mut self, _ctx: &mut NativeCtx<'_>, done: TaskDone<SpawnOutcome<SessionConsumer>>) {
        if self.staged.as_ref() == Some(&done.into_output().canonical_name) {
            self.staged = None;
            let _ = self.born.send(());
        }
    }
}

/// Starts [`DataOnlyConsumer`]'s relayed self-bind.
#[aether_data::kind(name = "test.tcp.relay_bind_self", copy)]
struct RelayBindSelf;

/// What [`DataOnlyConsumer`] sends itself, so the turn that relays the bind
/// has the consumer as its sender and reply target.
#[aether_data::kind(name = "test.tcp.forward_bind_self", copy)]
struct ForwardBindSelf;

/// [`DataOnlyConsumer`]'s own forwarding row.
#[aether_actor::protocol]
trait ForwardingBind {
    fn forward(mail: ForwardBindSelf) -> Undeclared;
}

/// Handles `SessionData` but not `SessionClosed`, so it does not cover
/// [`TcpConsumer`] and cannot build `ctx.send::<TcpCapability>` of a
/// `BindListenerSelf` (ADR-0231 §11). It reaches the cap the way a relay
/// does: it proves the bytes through the boundary and forwards them from a
/// turn it sent itself, so it is the mail's sender and reply target, and it
/// forwards the cap's reply to the test.
struct DataOnlyConsumer {
    replies: mpsc::Sender<BindListenerResult>,
    data_frames: usize,
    me: Option<ProtocolRef<ForwardingBind>>,
}

#[actor(singleton, root, depends(TcpCapability))]
impl NativeActor for DataOnlyConsumer {
    const NAMESPACE: &'static str = "test.tcp.data_only_consumer";
    type Config = ();
    type Params = mpsc::Sender<BindListenerResult>;

    fn init(
        (): (),
        replies: mpsc::Sender<BindListenerResult>,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<Self, BootError> {
        Ok(Self { replies, data_frames: 0, me: None })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        let me = ctx.resolve_path(&ErasedActorPath::new(Self::NAMESPACE).expect("a canonical path"));
        self.me = me.ok().and_then(|me| ctx.cast(me));
        Ok(())
    }

    #[handler::tell]
    fn on_relay(&mut self, ctx: &mut NativeCtx<'_>, _mail: RelayBindSelf) {
        ctx.send_to(self.me.expect("the consumer cast itself at wire"), &ForwardBindSelf);
    }

    #[handler::unchecked(reason = "test: relays the self-bind, reply target pinned to this consumer")]
    fn on_forward(&mut self, ctx: &mut NativeCtx<'_, Self, Unchecked>, _mail: ForwardBindSelf) {
        let _ = self;
        let tcp = ErasedActorPath::new(TcpCapability::NAMESPACE).expect("a canonical path");
        let bind = BindListenerSelf { addr: "127.0.0.1:0".into(), name: Some("data-only".into()) };

        ctx.deliver_forwarded(
            ctx.accept_call(&tcp, <BindListenerSelf as Kind>::ID, bind.encode_into_bytes()).expect("the cap is live"),
        );
    }

    #[handler::tell]
    fn on_session_data(&mut self, _ctx: &mut NativeCtx<'_>, _mail: SessionData) {
        self.data_frames += 1;
    }

    #[handler::response]
    fn on_bind_result(&mut self, _ctx: &mut NativeCtx<'_>, result: BindListenerResult) {
        let _ = self.replies.send(result);
    }
}

/// Spawn a [`SessionConsumer`] at the root instance `key` and answer its
/// path narrowed to [`TcpConsumer`], with the receiver of its captures.
fn spawn_consumer(
    chassis: &PassiveChassis<TestChassis>,
    key: &str,
) -> (ProtocolPath<TcpConsumer>, mpsc::Receiver<CapturedSessionMail>) {
    let (captures, rx) = mpsc::channel();
    chassis
        .spawn_actor::<SessionConsumer>(Subname::Named(key), captures, ())
        .finish()
        .expect("the session consumer spawns");

    (ActorPath::<SessionConsumer>::instance(&LoadName::new(key).expect("a valid key")).narrow(), rx)
}

fn address(text: &str) -> ErasedActorPath {
    ErasedActorPath::new(text).expect("test address is a valid actor path")
}

fn framed_body(body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + body.len());
    let body_len = u32::try_from(body.len()).expect("test frame body fits the wire prefix");
    frame.extend_from_slice(&body_len.to_le_bytes());
    frame.extend_from_slice(body);
    frame
}

fn register_route_collision(registry: &Registry, canonical_name: &str) -> ErasedActorRef {
    registered_ref(registry, canonical_name, Arc::new(|dispatch: OwnedDispatch| dispatch.discharge()))
}

/// Send `mail` to `to` with the session reply target, wait for its chain to
/// settle, and decode the reply it sent as `R`.
fn drive_and_decode<K: Kind, R: Kind, I>(
    chassis: &PassiveChassis<TestChassis>,
    rx: &mpsc::Receiver<EgressEvent>,
    to: impl ChassisTarget<K, I>,
    mail: &K,
) -> R {
    send_and_settle(chassis, to, mail, Some(session_reply()));
    next_reply::<R>(rx, K::NAME).2
}

/// Issue 607 Phase 6a: bind → list → unbind round-trip on a
/// loopback port. Asserts the cap-local supervisor map
/// reflects every step (bound, listed, unbound).
#[test]
fn bind_then_list_then_unbind_roundtrip() {
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();

    // Bind to port 0 — let the OS pick a free port.
    let bind_reply: BindListenerResult =
        drive_and_decode(&chassis, &rx, tcp, &BindListener { addr: "127.0.0.1:0".into(), name: None, consumer: None });
    let (listener_name, local_port) = match bind_reply {
        BindListenerResult::Ok { listener_name, local_port, .. } => (listener_name, local_port),
        BindListenerResult::Err(error) => panic!("bind failed: {error:?}"),
    };
    assert_eq!(listener_name, local_port.to_string(), "default subname should be the bound port");
    assert!(local_port > 0, "OS-picked port should be non-zero");

    // List enumerates the one listener.
    let list_reply: ListListenersResult = drive_and_decode(&chassis, &rx, tcp, &ListListeners::default());
    assert_eq!(list_reply.listeners.len(), 1, "exactly one listener");
    let entry = &list_reply.listeners[0];
    assert_eq!(entry.name, listener_name);
    assert_eq!(entry.port, local_port);
    assert_eq!(entry.addr, "127.0.0.1:0");

    // Unbind — asynchronous reply via MonitorNotice.
    let unbind_reply: UnbindListenerResult =
        drive_and_decode(&chassis, &rx, tcp, &UnbindListener { listener_name: listener_name.clone() });
    match unbind_reply {
        UnbindListenerResult::Ok { listener_name: ln } => assert_eq!(ln, listener_name),
        UnbindListenerResult::Err { error, .. } => panic!("unbind failed: {error}"),
    }

    // List should now be empty — cap-local supervisor map
    // dropped the entry on MonitorNotice.
    let list_reply: ListListenersResult = drive_and_decode(&chassis, &rx, tcp, &ListListeners::default());
    assert!(list_reply.listeners.is_empty(), "list should drop the unbound listener");
}

#[test]
fn staged_bind_reply_preserves_the_original_root_and_follows_monitor_commit() {
    const LISTENER_NAME: &str = "held-bind";
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();
    let session = SessionToken(Uuid::from_u128(0x4066_B1AD));
    let correlation_id = 0x4066;
    send_and_settle(
        &chassis,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some(LISTENER_NAME.into()), consumer: None },
        Some(ReplyTarget::Session { session, correlation: correlation_id }),
    );

    let (reply_session, reply_correlation_id, reply) = next_reply::<BindListenerResult>(&rx, "the staged bind reply");
    assert_eq!(reply_session, session);
    assert_eq!(reply_correlation_id, correlation_id);
    let BindListenerResult::Ok { listener_name, local_port, .. } = reply else {
        panic!("staged bind should succeed");
    };
    assert_eq!(listener_name, LISTENER_NAME);

    let listed: ListListenersResult = drive_and_decode(&chassis, &rx, tcp, &ListListeners::default());
    assert!(
        listed.listeners.iter().any(|entry| entry.name == LISTENER_NAME && entry.port == local_port),
        "the success reply is sent only after monitor installation and supervisor-map commit",
    );
    let unbound: UnbindListenerResult =
        drive_and_decode(&chassis, &rx, tcp, &UnbindListener { listener_name: LISTENER_NAME.into() });
    assert!(matches!(unbound, UnbindListenerResult::Ok { .. }));
}

#[test]
fn staged_bind_rejection_replies_once_and_releases_the_name() {
    const LISTENER_NAME: &str = "owner-rejected-listener";
    let canonical_name = format!("{}/{}:{LISTENER_NAME}", TcpCapability::NAMESPACE, TcpListenerActor::NAMESPACE);
    let (registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();
    let collision = register_route_collision(&registry, &canonical_name);

    let rejected: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some(LISTENER_NAME.into()), consumer: None },
    );
    assert!(
        matches!(rejected, BindListenerResult::Err(BindListenerError::Failed { ref addr, ref error })
            if addr == "127.0.0.1:0" && error.contains("spawn failed")),
        "owner rejection returns one typed bind failure: {rejected:?}",
    );
    assert!(rx.try_recv().is_err(), "authoritative rejection emits exactly one bind result");

    withdraw_ref(&registry, collision);

    let retried: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some(LISTENER_NAME.into()), consumer: None },
    );
    assert!(
        matches!(retried, BindListenerResult::Ok { ref listener_name, .. } if listener_name == LISTENER_NAME),
        "the rejected parent-local reservation is released for retry: {retried:?}",
    );

    let unbound: UnbindListenerResult =
        drive_and_decode(&chassis, &rx, tcp, &UnbindListener { listener_name: LISTENER_NAME.into() });
    assert!(matches!(unbound, UnbindListenerResult::Ok { .. }), "retry listener shuts down cleanly");
}

#[test]
fn duplicate_staged_listener_name_keeps_one_socket_and_rejects_the_other() {
    const LISTENER_NAME: &str = "duplicate-staged-listener";
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();
    let session_alpha = SessionToken(Uuid::from_u128(0x4066_DA1A));
    let session_beta = SessionToken(Uuid::from_u128(0x4066_DB7A));

    let (_, settled_alpha) = chassis.send_tracked(
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some(LISTENER_NAME.into()), consumer: None },
        Some(ReplyTarget::Session { session: session_alpha, correlation: 1 }),
    );
    let (_, settled_beta) = chassis.send_tracked(
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some(LISTENER_NAME.into()), consumer: None },
        Some(ReplyTarget::Session { session: session_beta, correlation: 2 }),
    );
    await_settled(&settled_alpha, "the alpha duplicate-name bind");
    await_settled(&settled_beta, "the beta duplicate-name bind");

    let replies: Vec<_> = (0..2)
        .map(|_| {
            let (session, _, result) = next_reply::<BindListenerResult>(&rx, "a duplicate-name bind result");
            (session, result)
        })
        .collect();
    assert!(replies.iter().any(|(session, _)| *session == session_alpha), "alpha receives its own result");
    assert!(replies.iter().any(|(session, _)| *session == session_beta), "beta receives its own result");

    let successes: Vec<_> = replies
        .iter()
        .filter_map(|(_, result)| match result {
            BindListenerResult::Ok { local_port, .. } => Some(*local_port),
            BindListenerResult::Err(_) => None,
        })
        .collect();
    let failures: Vec<_> = replies
        .iter()
        .filter_map(|(_, result)| match result {
            BindListenerResult::Err(BindListenerError::Failed { error, .. }) => Some(error.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(successes.len(), 1, "one staged child owns the parent-local name: {replies:?}");
    assert_eq!(failures.len(), 1, "the duplicate staged child receives one rejection: {replies:?}");
    assert!(failures[0].contains("spawn failed"), "the duplicate is rejected by staged spawn authority");

    let live_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), successes[0]);
    assert!(TcpListener::bind(live_addr).is_err(), "the accepted listener retains its socket");

    let unbound: UnbindListenerResult =
        drive_and_decode(&chassis, &rx, tcp, &UnbindListener { listener_name: LISTENER_NAME.into() });
    assert!(matches!(unbound, UnbindListenerResult::Ok { .. }));
}

#[test]
#[allow(clippy::disallowed_methods)] // test-only loopback server thread; no actor lineage or runtime work.
fn staged_connect_rejection_closes_the_stream_and_replies_once() {
    const SESSION_NAME: &str = "owner-rejected-session";
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind rejection probe server");
    let socket_addr = listener.local_addr().expect("rejection probe address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept the staged outbound stream");
        // A plain socket thread outside the engine has no settlement to wait
        // on; the read timeout only keeps an unclosed socket from hanging the
        // join.
        stream.set_read_timeout(Some(Duration::from_secs(2))).expect("bound rejection wait");
        let mut byte = [0_u8; 1];
        stream.read(&mut byte).expect("rejected prepared session closes its socket")
    });

    let canonical_name = format!("{}/{}:{SESSION_NAME}", TcpCapability::NAMESPACE, TcpSessionActor::NAMESPACE);
    let (registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();
    let _collision = register_route_collision(&registry, &canonical_name);
    let rejected: ConnectResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &Connect { addr: socket_addr.to_string(), name: Some(SESSION_NAME.into()), consumer: None },
    );

    assert!(
        matches!(rejected, ConnectResult::Err(ConnectError::Failed { ref addr, ref error })
            if addr == &socket_addr.to_string() && error.contains("spawn failed")),
        "owner rejection returns one typed connect failure: {rejected:?}",
    );
    assert_eq!(server.join().expect("rejection server completes"), 0, "the peer observes EOF after rollback");
    assert!(rx.try_recv().is_err(), "authoritative rejection emits exactly one connect result");
}

/// Issue 3051: asynchronous unbind retains the originating settlement root
/// until `MonitorNotice` sends exactly one result to the parked caller. The
/// reply keeps the original session/correlation and the root settles only
/// after that deferred reply has been emitted.
#[test]
fn unbind_monitor_reply_releases_the_originating_settlement_hold() {
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();
    let bind_reply: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("held-unbind".into()), consumer: None },
    );
    let listener_name = match bind_reply {
        BindListenerResult::Ok { listener_name, .. } => listener_name,
        BindListenerResult::Err(error) => panic!("bind failed: {error:?}"),
    };

    let session = SessionToken(Uuid::from_u128(0x3051));
    let correlation_id = 0x3051;
    send_and_settle(
        &chassis,
        tcp,
        &UnbindListener { listener_name: listener_name.clone() },
        Some(ReplyTarget::Session { session, correlation: correlation_id }),
    );

    let (reply_session, reply_correlation_id, reply) =
        next_reply::<UnbindListenerResult>(&rx, "the deferred unbind reply");
    assert_eq!(reply_session, session);
    assert_eq!(reply_correlation_id, correlation_id);
    match reply {
        UnbindListenerResult::Ok { listener_name: replied_name } => assert_eq!(replied_name, listener_name),
        UnbindListenerResult::Err { error, .. } => panic!("unbind failed: {error}"),
    }
    assert!(rx.try_recv().is_err(), "deferred unbind emits exactly one result");

    let listeners: ListListenersResult = drive_and_decode(&chassis, &rx, tcp, &ListListeners::default());
    assert!(listeners.listeners.is_empty(), "monitor cleanup removes the unbound listener");
}

/// Tripwire: the parked-unbind path. A second `UnbindListener` that lands
/// while the first close is still in flight must be told the unbind is in
/// progress and must not displace the first caller's parked reply.
///
/// The ordering is pinned rather than raced (iamacoffeepot/aether#4014). Both
/// unbinds are staged on the cap's inbox *before* the pump dispatches either,
/// so the FIFO drain runs the first turn — which parks the reply and only then
/// buffers `Close` to the listener — ahead of the duplicate. The listener's
/// `MonitorNotice`, the mail that retires the parked entry, cannot be minted
/// until that `Close` flushes, so it can never overtake a duplicate that is
/// already queued. Sending both to a pool-dispatched cap left the
/// duplicate racing the whole close chain, and a lost race took the
/// already-closed path instead of the parked one under CI load.
#[test]
fn duplicate_unbind_preserves_the_first_parked_reply() {
    let (registry, mailer, rx) = fresh_substrate();
    let mut cap = PumpedDriver::<TcpCapability>::boot(boot_bare_test_chassis(&registry, &mailer), (), ());
    let tcp = cap.chassis().actor_ref::<TcpCapability>();
    cap.send_and_settle(
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("duplicate-unbind".into()), consumer: None },
        Some(session_reply()),
    );
    let listener_name = match next_reply::<BindListenerResult>(&rx, "the bind reply").2 {
        BindListenerResult::Ok { listener_name, .. } => listener_name,
        BindListenerResult::Err(error) => panic!("bind failed: {error:?}"),
    };

    let first_session = SessionToken(Uuid::from_u128(0x3051_0001));
    let duplicate_session = SessionToken(Uuid::from_u128(0x3051_0002));
    let unbind = UnbindListener { listener_name: listener_name.clone() };
    let first = cap.send_tracked(tcp, &unbind, Some(ReplyTarget::Session { session: first_session, correlation: 1 }));
    let duplicate =
        cap.send_tracked(tcp, &unbind, Some(ReplyTarget::Session { session: duplicate_session, correlation: 2 }));
    cap.settle(&[first, duplicate]);

    let mut first_reply = None;
    let mut duplicate_reply = None;
    for _ in 0..2 {
        let (session, _, reply) = next_reply::<UnbindListenerResult>(&rx, "an unbind reply");
        if session == first_session {
            first_reply = Some(reply);
        } else if session == duplicate_session {
            duplicate_reply = Some(reply);
        } else {
            panic!("unbind replied to unexpected session {session:?}");
        }
    }

    assert!(
        matches!(first_reply, Some(UnbindListenerResult::Ok { listener_name: ref name }) if name == &listener_name),
        "the first caller retains the parked success reply: {first_reply:?}",
    );
    assert!(
        matches!(
            duplicate_reply,
            Some(UnbindListenerResult::Err { listener_name: ref name, ref error })
                if name == &listener_name && error == "unbind already in progress"
        ),
        "the duplicate caller receives the in-progress error: {duplicate_reply:?}",
    );
}

/// Tripwire: an outbound dial must correlate its parked reply to the
/// spawned cap-child session, and a `SessionWrite` to the proven
/// connect-side session must reach the dialed socket.
#[test]
#[allow(clippy::disallowed_methods)] // test-only loopback server thread; no actor lineage or runtime work.
fn connect_roundtrip_spawns_writable_session() {
    const REPLY: &[u8] = b"loopback-reply";
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
    let addr = listener.local_addr().expect("loopback server address");
    let framed_reply = framed_body(REPLY);
    let server_thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept connect-side session");
        stream.write_all(&framed_reply).expect("write framed reply to connect-side session");
        // A plain socket thread outside the engine has no settlement to wait
        // on; the read timeout only keeps a lost write from hanging the join.
        stream.set_read_timeout(Some(Duration::from_secs(2))).expect("set server read timeout");
        let mut received = [0_u8; 17];
        stream.read_exact(&mut received).expect("connect-side SessionWrite reaches loopback server");
        received
    });

    let (registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();
    let (consumer, consumer_rx) = spawn_consumer(&chassis, "connect-consumer");
    let connect_reply: ConnectResult =
        drive_and_decode(&chassis, &rx, tcp, &Connect { addr: addr.to_string(), name: None, consumer: Some(consumer) });
    let (session_name, peer) = match connect_reply {
        ConnectResult::Ok { session_name, peer } => (session_name, peer),
        ConnectResult::Err(error) => panic!("connect failed: {error:?}"),
    };
    assert!(!session_name.is_empty(), "connect result should name the spawned session");
    let session_path = format!("{}/{}:{session_name}", TcpCapability::NAMESPACE, TcpSessionActor::NAMESPACE);
    assert_eq!(
        registry.resolve_address(&address(&session_path)).map(|resolved| resolved.canonical_path),
        Ok(session_path),
        "the documented MCP lineage path resolves to the spawned session",
    );
    let peer = peer.parse::<SocketAddr>().expect("connect result peer is a socket address");
    assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST), "connect result peer should be on 127.0.0.1");

    // Session mail is driven by the socket, not by a root this test holds, so
    // the capture channel is the only signal and its wait is time-bounded.
    let received = consumer_rx.recv_timeout(Duration::from_secs(2)).expect("connect consumer receives SessionData");
    let CapturedSessionMail::Data(received) = received else {
        panic!("expected SessionData, got {received:?}");
    };
    assert_eq!(received.session_name, session_name);
    assert_eq!(received.peer, peer.to_string());
    assert_eq!(received.bytes, REPLY);

    let session = chassis
        .child::<TcpCapability, TcpSessionActor>(
            chassis.actor_ref::<TcpCapability>(),
            LoadName::new(&session_name).expect("session name is a load name"),
        )
        .expect("the connect-side session is live");
    send_and_settle(&chassis, session, &SessionWrite { bytes: b"connect-roundtrip".to_vec() }, None);

    assert_eq!(server_thread.join().expect("loopback server thread completes"), *b"connect-roundtrip");
}

/// Tripwire: two in-flight dials must keep their own caller, correlation,
/// settlement root, and result. Completing in either OS order must not cross
/// the parked replies.
#[test]
#[allow(clippy::disallowed_methods)] // test-only loopback server threads; no actor lineage or runtime work.
fn concurrent_connects_reply_to_their_own_origins() {
    let listener_alpha = TcpListener::bind("127.0.0.1:0").expect("bind alpha loopback server");
    let listener_beta = TcpListener::bind("127.0.0.1:0").expect("bind beta loopback server");
    let addr_alpha = listener_alpha.local_addr().expect("alpha loopback address");
    let addr_beta = listener_beta.local_addr().expect("beta loopback address");
    let server_alpha = thread::spawn(move || listener_alpha.accept().expect("accept alpha connect").1);
    let server_beta = thread::spawn(move || listener_beta.accept().expect("accept beta connect").1);

    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();
    let session_alpha = SessionToken(Uuid::from_u128(0xA11A));
    let session_beta = SessionToken(Uuid::from_u128(0xB37A));
    let correlation_alpha = 0xA11A;
    let correlation_beta = 0xB37A;
    let (_, settled_alpha) = chassis.send_tracked(
        tcp,
        &Connect { addr: addr_alpha.to_string(), name: Some("alpha".into()), consumer: None },
        Some(ReplyTarget::Session { session: session_alpha, correlation: correlation_alpha }),
    );
    let (_, settled_beta) = chassis.send_tracked(
        tcp,
        &Connect { addr: addr_beta.to_string(), name: Some("beta".into()), consumer: None },
        Some(ReplyTarget::Session { session: session_beta, correlation: correlation_beta }),
    );

    await_settled(&settled_alpha, "the alpha connect");
    await_settled(&settled_beta, "the beta connect");

    let mut saw_alpha = false;
    let mut saw_beta = false;
    for _ in 0..2 {
        let (session, correlation_id, reply) = next_reply::<ConnectResult>(&rx, "a concurrent connect reply");
        let ConnectResult::Ok { session_name, peer, .. } = reply else {
            panic!("concurrent connect should succeed");
        };
        let peer = peer.parse::<SocketAddr>().expect("connect peer is a socket address");
        if session == session_alpha {
            assert!(!saw_alpha, "alpha replies exactly once");
            assert_eq!(correlation_id, correlation_alpha);
            assert_eq!(session_name, "alpha");
            assert_eq!(peer.port(), addr_alpha.port());
            saw_alpha = true;
        } else if session == session_beta {
            assert!(!saw_beta, "beta replies exactly once");
            assert_eq!(correlation_id, correlation_beta);
            assert_eq!(session_name, "beta");
            assert_eq!(peer.port(), addr_beta.port());
            saw_beta = true;
        } else {
            panic!("reply reached an unexpected session");
        }
    }
    assert!(rx.try_recv().is_err(), "each staged connect emits exactly one result");
    assert!(server_alpha.join().expect("alpha server thread completes").ip().is_loopback());
    assert!(server_beta.join().expect("beta server thread completes").ip().is_loopback());
}

/// Binding the same port twice fails the second bind. Uses
/// the first bind's actually-bound port to drive the second.
#[test]
fn bind_port_in_use_returns_err() {
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();

    let first: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("first".into()), consumer: None },
    );
    let local_port = match first {
        BindListenerResult::Ok { local_port, .. } => local_port,
        BindListenerResult::Err(error) => panic!("first bind failed: {error:?}"),
    };

    // Second bind on the same port — must fail.
    let second: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: format!("127.0.0.1:{local_port}"), name: Some("second".into()), consumer: None },
    );
    match second {
        BindListenerResult::Err(BindListenerError::Failed { error, addr }) => {
            assert_eq!(addr, format!("127.0.0.1:{local_port}"));
            assert!(error.starts_with("bind failed:"), "expected bind-fail error, got: {error}");
        }
        other => panic!("expected port-in-use Err, got {other:?}"),
    }
}

/// Unbind on an unknown name surfaces an Err with the name
/// echoed back.
#[test]
fn unbind_unknown_listener_errors() {
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();

    let reply: UnbindListenerResult =
        drive_and_decode(&chassis, &rx, tcp, &UnbindListener { listener_name: "nope".into() });
    match reply {
        UnbindListenerResult::Err { listener_name, .. } => {
            assert_eq!(listener_name, "nope");
        }
        UnbindListenerResult::Ok { .. } => panic!("expected Err for unknown listener"),
    }
}

/// A refused consumer binds no socket and spawns no listener: a `consumer`
/// path no route has stood at is refused at decode, and the dispatch answers
/// the request `Err(Consumer(..))` naming the path, rather than dropping it or
/// binding a listener whose frames go nowhere. It is the one reply the
/// request gets, and the next the session sees is the list's, which lists
/// nothing.
#[test]
fn bind_refuses_a_consumer_path_with_no_live_covering_actor() {
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();
    let unregistered = ActorPath::<SessionConsumer>::instance(&LoadName::new("unregistered").expect("a valid key"));
    let consumer: ProtocolPath<TcpConsumer> = unregistered.narrow();

    let refused: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("orphan".into()), consumer: Some(consumer.clone()) },
    );
    assert!(
        matches!(&refused, BindListenerResult::Err(BindListenerError::Consumer(PathRefused { path, reason }))
            if path == consumer.as_erased() && *reason == PathRefusal::Unpublished),
        "the refused consumer is answered naming its path: {refused:?}",
    );
    assert!(rx.try_recv().is_err(), "a refused bind sends exactly one reply");

    let list: ListListenersResult = drive_and_decode(&chassis, &rx, tcp, &ListListeners::default());
    assert!(list.listeners.is_empty(), "a refused bind must spawn no listener: {:?}", list.listeners);
}

/// The engine casts a `BindListenerSelf`'s sender before the cap's handler
/// runs (ADR-0231 §11): relayed from an actor that handles `SessionData` but
/// not `SessionClosed`, it is answered once with `Err(Consumer(..))` naming
/// that actor and the row it lacks, and binds nothing. Fails if the handler
/// runs for a sender the build could not check, which would bind a listener
/// whose sessions' close notices its consumer warn-drops, or if the refused
/// request is left unanswered.
#[test]
fn bind_listener_self_refuses_a_relayed_sender_that_does_not_cover_the_consumer_protocol() {
    let (replies_tx, replies) = mpsc::channel();
    let (_registry, _mailer, rx, chassis) =
        boot_tcp_substrate_with(|builder| builder.with_actor::<DataOnlyConsumer>(replies_tx));
    let tcp = chassis.actor_ref::<TcpCapability>();
    let consumer = chassis.actor_ref::<DataOnlyConsumer>();

    send_and_settle(&chassis, consumer, &RelayBindSelf, None);

    let sender = ErasedActorPath::new(DataOnlyConsumer::NAMESPACE).expect("a canonical path");
    let lacked = PathRefusal::Uncovered { kind: <SessionClosed as Kind>::ID };
    let reply = replies.try_recv().expect("the refused self-bind is answered before its chain settles");
    assert!(
        matches!(&reply, BindListenerResult::Err(BindListenerError::Consumer(PathRefused { path, reason }))
            if *path == sender && *reason == lacked),
        "the refused sender is answered naming it and the row it lacks: {reply:?}",
    );
    assert!(replies.try_recv().is_err(), "a refused self-bind sends exactly one reply");

    let list: ListListenersResult = drive_and_decode(&chassis, &rx, tcp, &ListListeners::default());
    assert!(list.listeners.is_empty(), "a refused bind must spawn no listener: {:?}", list.listeners);
}

/// A bound consumer receives one mail per complete frame even when a
/// frame body spans TCP writes, followed by a close notice on peer EOF.
#[test]
fn session_reassembles_frames_for_bound_consumer_and_reports_eof() {
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();
    let (consumer, consumer_rx) = spawn_consumer(&chassis, "delivery-consumer");

    let bind: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("delivery".into()), consumer: Some(consumer) },
    );
    let local_port = match bind {
        BindListenerResult::Ok { local_port, .. } => local_port,
        BindListenerResult::Err(error) => panic!("bind failed: {error:?}"),
    };

    let first_body = b"first complete frame";
    let second_body = b"second body split across writes";
    let first_frame = framed_body(first_body);
    let second_frame = framed_body(second_body);
    let second_split = 4 + 7;
    let mut client = TcpStream::connect(("127.0.0.1", local_port)).expect("connect loopback client");
    let mut first_write = first_frame;
    first_write.extend_from_slice(&second_frame[..second_split]);
    client.write_all(&first_write).expect("write first frame and partial second frame");

    // Session mail is driven by the socket, not by a root this test holds, so
    // the capture channel is the only signal and its wait is time-bounded.
    let first = consumer_rx.recv_timeout(Duration::from_secs(2)).expect("first SessionData arrives");
    let CapturedSessionMail::Data(first) = first else {
        panic!("expected first SessionData, got {first:?}");
    };
    assert_eq!(first.session_name, "conn-0");
    assert_eq!(first.bytes, first_body);
    assert!(consumer_rx.try_recv().is_err(), "partial second frame must not be delivered");

    client.write_all(&second_frame[second_split..]).expect("complete second frame");
    let second = consumer_rx.recv_timeout(Duration::from_secs(2)).expect("second SessionData arrives");
    let CapturedSessionMail::Data(second) = second else {
        panic!("expected second SessionData, got {second:?}");
    };
    assert_eq!(second.session_name, "conn-0");
    assert_eq!(second.peer, first.peer);
    assert_eq!(second.bytes, second_body);

    drop(client);
    let closed = consumer_rx.recv_timeout(Duration::from_secs(2)).expect("SessionClosed arrives on EOF");
    let CapturedSessionMail::Closed(closed) = closed else {
        panic!("expected SessionClosed, got {closed:?}");
    };
    assert_eq!(closed.session_name, "conn-0");
    assert_eq!(closed.peer, second.peer);
    assert_eq!(closed.reason, "eof");
    // The close chain starts at socket EOF, not at a root this test holds,
    // so there is no settlement to wait on: a short quiet window is the only
    // way to see that no further mail follows the close.
    thread::sleep(Duration::from_millis(50));
    assert!(consumer_rx.try_recv().is_err(), "consumer must receive exactly two data mails and one close mail");
}

/// Tripwire: a consumer that is a *nested* actor still receives its
/// session mail. A wasm component loaded beneath a parent lives at the
/// ADR-0099 lineage path `parent/NS:key` (ADR-0241 §5), which is precisely
/// what the `consumer` field exists to serve. The written lineage path
/// is proven at decode through the registry's fold, so a nested consumer
/// is reachable; resolving it as a flat name would refuse every bind
/// naming a component.
#[test]
fn nested_lineage_consumer_receives_session_mail() {
    let (captures, consumer_rx) = mpsc::channel();
    let (born, born_rx) = mpsc::channel();
    let (_registry, _mailer, rx, chassis) =
        boot_tcp_substrate_with(|builder| builder.with_actor::<ConsumerHost>((captures, born)));
    let tcp = chassis.actor_ref::<TcpCapability>();
    let key = LoadName::new(NESTED_CONSUMER_KEY).expect("a valid key");
    born_rx
        .recv_timeout(SettlementConfig::from_env().to_cap())
        .expect("the nested consumer's birth was never decided within the settlement cap");
    chassis
        .child::<ConsumerHost, SessionConsumer>(chassis.actor_ref::<ConsumerHost>(), key.clone())
        .expect("the nested consumer is live");
    let consumer = ActorPath::<SessionConsumer>::child(&ActorPath::<ConsumerHost>::root(), &key)
        .expect("the nested path is under the caps");

    let bind: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("nested".into()), consumer: Some(consumer.narrow()) },
    );
    let local_port = match bind {
        BindListenerResult::Ok { local_port, .. } => local_port,
        BindListenerResult::Err(error) => panic!("bind failed: {error:?}"),
    };

    let body = b"frame for a nested consumer";
    let mut client = TcpStream::connect(("127.0.0.1", local_port)).expect("connect loopback client");
    client.write_all(&framed_body(body)).expect("write one complete frame");

    // Session mail is driven by the socket, not by a root this test holds, so
    // the capture channel is the only signal and its wait is time-bounded.
    let delivered = consumer_rx.recv_timeout(Duration::from_secs(2)).expect("SessionData reaches a nested consumer");
    let CapturedSessionMail::Data(delivered) = delivered else {
        panic!("expected SessionData, got {delivered:?}");
    };
    assert_eq!(delivered.bytes, body);
}

/// Rejecting an invalid frame is an observable session close, not a
/// silent shutdown: the bound consumer receives exactly one close notice.
#[test]
fn session_reports_frame_rejection_to_bound_consumer() {
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();
    let (consumer, consumer_rx) = spawn_consumer(&chassis, "rejection-consumer");

    let bind: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("rejection".into()), consumer: Some(consumer) },
    );
    let local_port = match bind {
        BindListenerResult::Ok { local_port, .. } => local_port,
        BindListenerResult::Err(error) => panic!("bind failed: {error:?}"),
    };

    let mut client = TcpStream::connect(("127.0.0.1", local_port)).expect("connect loopback client");
    client.write_all(&u32::MAX.to_le_bytes()).expect("write oversize frame prefix");

    // Session mail is driven by the socket, not by a root this test holds, so
    // the capture channel is the only signal and its wait is time-bounded.
    let closed = consumer_rx.recv_timeout(Duration::from_secs(2)).expect("SessionClosed arrives on frame rejection");
    let CapturedSessionMail::Closed(closed) = closed else {
        panic!("expected SessionClosed, got {closed:?}");
    };
    assert_eq!(closed.session_name, "conn-0");
    assert!(closed.peer.starts_with("127.0.0.1:"));
    assert!(closed.reason.starts_with("frame rejected: frame too large:"), "unexpected reason: {}", closed.reason);
    // The close chain starts at the rejected read, not at a root this test
    // holds, so there is no settlement to wait on: a short quiet window is
    // the only way to see that no second close follows.
    thread::sleep(Duration::from_millis(50));
    assert!(consumer_rx.try_recv().is_err(), "consumer must receive exactly one close mail");
}

/// Two concurrent binds on different ports both surface in
/// `ListListeners`.
#[test]
fn list_enumerates_two_concurrent_listeners() {
    let (_registry, _mailer, rx, chassis) = boot_tcp_substrate();
    let tcp = chassis.actor_ref::<TcpCapability>();

    let _: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("admin".into()), consumer: None },
    );
    let _: BindListenerResult = drive_and_decode(
        &chassis,
        &rx,
        tcp,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("game".into()), consumer: None },
    );

    let list: ListListenersResult = drive_and_decode(&chassis, &rx, tcp, &ListListeners::default());
    let mut names: Vec<String> = list.listeners.iter().map(|l| l.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["admin".to_string(), "game".to_string()]);
}
