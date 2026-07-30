//! Tests for the `aether.tcp` control plane: connect / bind / list / unbind
//! round-trips through a passive chassis with a real loopback socket.
#![allow(
    clippy::disallowed_methods,
    reason = "these tests register and address session consumers by rendered lineage path — the nested-name registration surface under test"
)]

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{
    BindListener, BindListenerResult, Connect, ConnectResult, ListListeners, ListListenersResult, SessionClosed,
    SessionData, TcpCapability, TcpListenerActor, TcpNativeExt, TcpSessionActor, UnbindListener, UnbindListenerResult,
};
use aether_actor::Addressable;
use aether_data::{Kind, MailboxId, SessionToken, Uuid, mailbox_id_from_path};
use aether_kinds::descriptors;
use aether_kinds::trace::Nanos;
use aether_substrate::actor::native::NativeActorMailbox;
use aether_substrate::actor::native::binding::NativeBinding;
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::mail::MailId;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
use aether_substrate::mail::registry::OwnedDispatch;
use aether_substrate::mail::registry::{MailboxEntry, Registry};
use aether_substrate::mail::{MailRef, Source, SourceAddr};
use aether_substrate::testing::TestChassis;

fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>, mpsc::Receiver<EgressEvent>) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
    }
    let (outbound, rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    (registry, mailer, rx)
}

/// Boot a fresh substrate with `TcpCapability` registered as a
/// passive actor and return the pieces every test in this
/// module reaches for: the kind registry (for mailbox lookup
/// in [`drive_and_decode`]), the egress receiver (for reply
/// decode), and the [`PassiveChassis`] (held by the caller so
/// the cap's actor thread stays alive for the test body).
///
/// Collapses the previously-duplicated `fresh_substrate()` +
/// `Builder::<TestChassis>::new(...)` chain that opened every
/// test (issue 796).
fn boot_tcp_substrate() -> (Arc<Registry>, Arc<Mailer>, mpsc::Receiver<EgressEvent>, PassiveChassis<TestChassis>) {
    let (registry, mailer, rx) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TcpCapability>(())
        .build_passive()
        .expect("TcpCapability boots");
    (registry, mailer, rx, chassis)
}

fn session_reply() -> Source {
    Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xfeed))))
}

fn enqueue<K: Kind>(registry: &Arc<Registry>, cap_namespace: &str, mail: &K, source: Source, root: MailId) {
    let id = registry.lookup(cap_namespace).expect("cap mailbox registered");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("cap entry") else {
        panic!("expected mailbox entry");
    };
    handler.enqueue(OwnedDispatch::disarmed(
        K::ID,
        K::NAME.to_owned(),
        None,
        source,
        MailRef::from(mail.encode_into_bytes()),
        1,
        root,
        root,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
}

#[derive(Debug)]
enum CapturedSessionMail {
    Data(SessionData),
    Closed(SessionClosed),
}

/// Register a capture inbox for a session consumer. Registers under the
/// ADR-0099 lineage fold of `name` (`try_register_inbox_with_id`) rather
/// than `hash(name)`, so `name` may be a nested path — the shape a loaded
/// wasm component has, and the shape the `consumer` field must serve.
fn register_session_consumer(registry: &Registry, name: &str) -> mpsc::Receiver<CapturedSessionMail> {
    let (tx, rx) = mpsc::channel();
    registry
        .try_register_inbox_with_id(
            mailbox_id_from_path(name),
            name,
            Arc::new(move |dispatch: OwnedDispatch| {
                let captured = if dispatch.kind == SessionData::ID {
                    CapturedSessionMail::Data(
                        SessionData::decode_from_bytes(dispatch.payload.bytes()).expect("decode SessionData"),
                    )
                } else if dispatch.kind == SessionClosed::ID {
                    CapturedSessionMail::Closed(
                        SessionClosed::decode_from_bytes(dispatch.payload.bytes()).expect("decode SessionClosed"),
                    )
                } else {
                    panic!("unexpected consumer kind: {}", dispatch.kind);
                };
                dispatch.discharge();
                tx.send(captured).expect("capture receiver remains live");
            }),
        )
        .expect("register session consumer");
    rx
}

fn framed_body(body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + body.len());
    let body_len = u32::try_from(body.len()).expect("test frame body fits the wire prefix");
    frame.extend_from_slice(&body_len.to_le_bytes());
    frame.extend_from_slice(body);
    frame
}

fn available_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve a loopback address");
    let addr = listener.local_addr().expect("reserved loopback address");
    drop(listener);
    addr
}

fn register_route_collision(registry: &Registry, canonical_name: &str) -> MailboxId {
    let id = mailbox_id_from_path(canonical_name);
    registry
        .try_register_inbox_with_id(id, canonical_name, Arc::new(|dispatch: OwnedDispatch| dispatch.discharge()))
        .expect("install the test-only collision authority");
    id
}

/// Push an encoded mail (via the kind's `encode_into_bytes`) at
/// the cap's mailbox via the registered sink handler, then wait
/// for the next outbound reply on `rx` and decode as `R`.
fn drive_and_decode<K, R>(
    registry: &Arc<Registry>,
    rx: &mpsc::Receiver<EgressEvent>,
    cap_namespace: &str,
    mail: &K,
) -> R
where
    K: Kind,
    R: Kind,
{
    enqueue(registry, cap_namespace, mail, session_reply(), MailId::NONE);

    let deadline = Instant::now() + Duration::from_secs(2);
    let frame = loop {
        if let Ok(f) = rx.try_recv() {
            break f;
        }
        assert!(Instant::now() < deadline, "reply did not arrive within deadline for {}", K::NAME);
        thread::sleep(Duration::from_millis(5));
    };
    let payload = match frame {
        EgressEvent::ToSession { payload, .. } => payload,
        other => panic!("expected ToSession egress, got {other:?}"),
    };
    R::decode_from_bytes(&payload).expect("decode reply")
}

/// Issue 607 Phase 6a: bind → list → unbind round-trip on a
/// loopback port. Asserts the cap-local supervisor map
/// reflects every step (bound, listed, unbound).
#[test]
fn bind_then_list_then_unbind_roundtrip() {
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();

    // Bind to port 0 — let the OS pick a free port.
    let bind_reply: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: "127.0.0.1:0".into(), name: None, consumer: None },
    );
    let (listener_name, local_port) = match bind_reply {
        BindListenerResult::Ok { listener_name, local_port, .. } => (listener_name, local_port),
        BindListenerResult::Err { reason, .. } => panic!("bind failed: {reason}"),
    };
    assert_eq!(listener_name, local_port.to_string(), "default subname should be the bound port");
    assert!(local_port > 0, "OS-picked port should be non-zero");

    // List enumerates the one listener.
    let list_reply: ListListenersResult =
        drive_and_decode(&registry, &rx, TcpCapability::NAMESPACE, &ListListeners::default());
    assert_eq!(list_reply.listeners.len(), 1, "exactly one listener");
    let entry = &list_reply.listeners[0];
    assert_eq!(entry.name, listener_name);
    assert_eq!(entry.port, local_port);
    assert_eq!(entry.addr, "127.0.0.1:0");

    // Unbind — asynchronous reply via MonitorNotice.
    let unbind_reply: UnbindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &UnbindListener { listener_name: listener_name.clone() },
    );
    match unbind_reply {
        UnbindListenerResult::Ok { listener_name: ln } => assert_eq!(ln, listener_name),
        UnbindListenerResult::Err { reason, .. } => panic!("unbind failed: {reason}"),
    }

    // List should now be empty — cap-local supervisor map
    // dropped the entry on MonitorNotice.
    let list_reply: ListListenersResult =
        drive_and_decode(&registry, &rx, TcpCapability::NAMESPACE, &ListListeners::default());
    assert!(list_reply.listeners.is_empty(), "list should drop the unbound listener");
}

#[test]
fn staged_bind_reply_preserves_the_original_root_and_follows_monitor_commit() {
    const LISTENER_NAME: &str = "held-bind";
    let (registry, mailer, rx, chassis) = boot_tcp_substrate();
    let session = SessionToken(Uuid::from_u128(0x4066_B1AD));
    let correlation_id = 0x4066;
    let root = MailId::new(MailboxId(0x4066_B1AD), correlation_id);
    let settled = chassis.settlement_registry().subscribe_settlement(root);
    let cap_id = registry.lookup(TcpCapability::NAMESPACE).expect("cap mailbox registered");
    mailer.record_sent(root, root, None, root.sender, cap_id, BindListener::ID);
    enqueue(
        &registry,
        TcpCapability::NAMESPACE,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some(LISTENER_NAME.into()), consumer: None },
        Source::with_correlation(SourceAddr::Session(session), correlation_id),
        root,
    );

    let event = rx.recv_timeout(Duration::from_secs(2)).expect("staged bind reply arrives");
    let EgressEvent::ToSession {
        session: reply_session, kind_name, payload, correlation_id: reply_correlation_id, ..
    } = event
    else {
        panic!("expected staged bind reply to the originating session");
    };
    assert_eq!(reply_session, session);
    assert_eq!(reply_correlation_id, correlation_id);
    assert_eq!(kind_name, BindListenerResult::NAME);
    let BindListenerResult::Ok { listener_name, local_port, .. } =
        BindListenerResult::decode_from_bytes(&payload).expect("decode staged BindListenerResult")
    else {
        panic!("staged bind should succeed");
    };
    assert_eq!(listener_name, LISTENER_NAME);
    settled.recv_timeout(Duration::from_secs(2)).expect("the original root settles after the staged reply");

    let listed: ListListenersResult =
        drive_and_decode(&registry, &rx, TcpCapability::NAMESPACE, &ListListeners::default());
    assert!(
        listed.listeners.iter().any(|entry| entry.name == LISTENER_NAME && entry.port == local_port),
        "the success reply is sent only after monitor installation and supervisor-map commit",
    );
    let unbound: UnbindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &UnbindListener { listener_name: LISTENER_NAME.into() },
    );
    assert!(matches!(unbound, UnbindListenerResult::Ok { .. }));
}

#[test]
fn staged_bind_rejection_closes_the_socket_replies_once_and_releases_the_name() {
    const LISTENER_NAME: &str = "owner-rejected-listener";
    let socket_addr = available_loopback_addr();
    let canonical_name = format!("{}/{}:{LISTENER_NAME}", TcpCapability::NAMESPACE, TcpListenerActor::NAMESPACE);
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();
    let collision_id = register_route_collision(&registry, &canonical_name);

    let rejected: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: socket_addr.to_string(), name: Some(LISTENER_NAME.into()), consumer: None },
    );
    assert!(
        matches!(rejected, BindListenerResult::Err { ref addr, ref reason }
            if addr == &socket_addr.to_string() && reason.contains("spawn failed")),
        "owner rejection returns one typed bind failure: {rejected:?}",
    );
    assert!(
        matches!(rx.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout)),
        "authoritative rejection emits exactly one bind result",
    );

    let rebound =
        TcpListener::bind(socket_addr).expect("the prepared listener socket is dropped before failure completion");
    drop(rebound);
    registry.drop_mailbox(collision_id).expect("remove test-only collision route");

    let retried: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: socket_addr.to_string(), name: Some(LISTENER_NAME.into()), consumer: None },
    );
    assert!(
        matches!(retried, BindListenerResult::Ok { ref listener_name, local_port, .. }
            if listener_name == LISTENER_NAME && local_port == socket_addr.port()),
        "the rejected parent-local reservation is released for retry: {retried:?}",
    );

    let unbound: UnbindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &UnbindListener { listener_name: LISTENER_NAME.into() },
    );
    assert!(matches!(unbound, UnbindListenerResult::Ok { .. }), "retry listener shuts down cleanly");
}

#[test]
fn duplicate_staged_listener_name_keeps_one_socket_and_rejects_the_other() {
    const LISTENER_NAME: &str = "duplicate-staged-listener";
    let reservation_alpha = TcpListener::bind("127.0.0.1:0").expect("reserve alpha duplicate-name socket");
    let reservation_beta = TcpListener::bind("127.0.0.1:0").expect("reserve beta duplicate-name socket");
    let addr_alpha = reservation_alpha.local_addr().expect("alpha duplicate-name address");
    let addr_beta = reservation_beta.local_addr().expect("beta duplicate-name address");
    drop(reservation_alpha);
    drop(reservation_beta);
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();
    let session_alpha = SessionToken(Uuid::from_u128(0x4066_DA1A));
    let session_beta = SessionToken(Uuid::from_u128(0x4066_DB7A));

    enqueue(
        &registry,
        TcpCapability::NAMESPACE,
        &BindListener { addr: addr_alpha.to_string(), name: Some(LISTENER_NAME.into()), consumer: None },
        Source::with_correlation(SourceAddr::Session(session_alpha), 1),
        MailId::NONE,
    );
    enqueue(
        &registry,
        TcpCapability::NAMESPACE,
        &BindListener { addr: addr_beta.to_string(), name: Some(LISTENER_NAME.into()), consumer: None },
        Source::with_correlation(SourceAddr::Session(session_beta), 2),
        MailId::NONE,
    );

    let mut replies = Vec::new();
    for _ in 0..2 {
        let EgressEvent::ToSession { session, kind_name, payload, .. } =
            rx.recv_timeout(Duration::from_secs(2)).expect("both duplicate-name callers receive a result")
        else {
            panic!("expected duplicate bind reply to a session");
        };
        assert_eq!(kind_name, BindListenerResult::NAME);
        replies.push((session, BindListenerResult::decode_from_bytes(&payload).expect("decode duplicate bind result")));
    }
    assert!(replies.iter().any(|(session, _)| *session == session_alpha), "alpha receives its own result");
    assert!(replies.iter().any(|(session, _)| *session == session_beta), "beta receives its own result");

    let successes: Vec<_> = replies
        .iter()
        .filter_map(|(_, result)| match result {
            BindListenerResult::Ok { local_port, .. } => Some(*local_port),
            BindListenerResult::Err { .. } => None,
        })
        .collect();
    let failures: Vec<_> = replies
        .iter()
        .filter_map(|(_, result)| match result {
            BindListenerResult::Err { addr, reason } => Some((addr.clone(), reason.clone())),
            BindListenerResult::Ok { .. } => None,
        })
        .collect();
    assert_eq!(successes.len(), 1, "one staged child owns the parent-local name: {replies:?}");
    assert_eq!(failures.len(), 1, "the duplicate staged child receives one rejection: {replies:?}");
    assert!(failures[0].1.contains("spawn failed"), "the duplicate is rejected by staged spawn authority");

    let failed_addr = failures[0].0.parse::<SocketAddr>().expect("failed bind address remains parseable");
    let rebound = TcpListener::bind(failed_addr).expect("the rejected duplicate's socket is closed before its reply");
    drop(rebound);
    let live_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), successes[0]);
    assert!(TcpListener::bind(live_addr).is_err(), "the accepted listener retains its socket");

    let unbound: UnbindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &UnbindListener { listener_name: LISTENER_NAME.into() },
    );
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
        stream.set_read_timeout(Some(Duration::from_secs(2))).expect("bound rejection wait");
        let mut byte = [0_u8; 1];
        stream.read(&mut byte).expect("rejected prepared session closes its socket")
    });

    let canonical_name = format!("{}/{}:{SESSION_NAME}", TcpCapability::NAMESPACE, TcpSessionActor::NAMESPACE);
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();
    let _collision_id = register_route_collision(&registry, &canonical_name);
    let rejected: ConnectResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &Connect { addr: socket_addr.to_string(), name: Some(SESSION_NAME.into()), consumer: None },
    );

    assert!(
        matches!(rejected, ConnectResult::Err { ref addr, ref reason }
            if addr == &socket_addr.to_string() && reason.contains("spawn failed")),
        "owner rejection returns one typed connect failure: {rejected:?}",
    );
    assert_eq!(server.join().expect("rejection server completes"), 0, "the peer observes EOF after rollback");
    assert!(
        matches!(rx.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout)),
        "authoritative rejection emits exactly one connect result",
    );
}

/// Issue 3051: asynchronous unbind retains the originating settlement root
/// until `MonitorNotice` sends exactly one result to the parked caller. The
/// reply keeps the original session/correlation and the root settles only
/// after that deferred reply has been emitted.
#[test]
fn unbind_monitor_reply_releases_the_originating_settlement_hold() {
    let (registry, mailer, rx, chassis) = boot_tcp_substrate();
    let bind_reply: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("held-unbind".into()), consumer: None },
    );
    let listener_name = match bind_reply {
        BindListenerResult::Ok { listener_name, .. } => listener_name,
        BindListenerResult::Err { reason, .. } => panic!("bind failed: {reason}"),
    };

    let session = SessionToken(Uuid::from_u128(0x3051));
    let correlation_id = 0x3051;
    let root = MailId::new(MailboxId(0x3051), correlation_id);
    let settled = chassis.settlement_registry().subscribe_settlement(root);
    let cap_id = registry.lookup(TcpCapability::NAMESPACE).expect("cap mailbox registered");
    mailer.record_sent(root, root, None, root.sender, cap_id, UnbindListener::ID);
    let MailboxEntry::Inbox { handler, .. } = registry.entry(cap_id).expect("cap entry") else {
        panic!("expected cap inbox");
    };
    let unbind = UnbindListener { listener_name: listener_name.clone() };
    handler.enqueue(OwnedDispatch::disarmed(
        UnbindListener::ID,
        UnbindListener::NAME.to_owned(),
        None,
        Source::with_correlation(SourceAddr::Session(session), correlation_id),
        MailRef::from(unbind.encode_into_bytes()),
        1,
        root,
        root,
        None,
        Nanos(0),
        0,
        cap_id,
    ));

    let event = rx.recv_timeout(Duration::from_secs(2)).expect("deferred unbind reply arrives before deadline");
    let EgressEvent::ToSession {
        session: reply_session, kind_name, payload, correlation_id: reply_correlation_id, ..
    } = event
    else {
        panic!("expected deferred unbind reply to the originating session");
    };
    assert_eq!(reply_session, session);
    assert_eq!(reply_correlation_id, correlation_id);
    assert_eq!(kind_name, UnbindListenerResult::NAME);
    match UnbindListenerResult::decode_from_bytes(&payload).expect("decode deferred UnbindListenerResult") {
        UnbindListenerResult::Ok { listener_name: replied_name } => assert_eq!(replied_name, listener_name),
        UnbindListenerResult::Err { reason, .. } => panic!("unbind failed: {reason}"),
    }

    settled.recv_timeout(Duration::from_secs(2)).expect("originating root settles after deferred reply");
    assert!(
        matches!(rx.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout)),
        "deferred unbind emits exactly one result",
    );
    let listeners: ListListenersResult =
        drive_and_decode(&registry, &rx, TcpCapability::NAMESPACE, &ListListeners::default());
    assert!(listeners.listeners.is_empty(), "monitor cleanup removes the unbound listener");
}

#[test]
fn duplicate_unbind_preserves_the_first_parked_reply() {
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();
    let bind_reply: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("duplicate-unbind".into()), consumer: None },
    );
    let listener_name = match bind_reply {
        BindListenerResult::Ok { listener_name, .. } => listener_name,
        BindListenerResult::Err { reason, .. } => panic!("bind failed: {reason}"),
    };

    let first_session = SessionToken(Uuid::from_u128(0x3051_0001));
    let duplicate_session = SessionToken(Uuid::from_u128(0x3051_0002));
    let unbind = UnbindListener { listener_name: listener_name.clone() };
    enqueue(
        &registry,
        TcpCapability::NAMESPACE,
        &unbind,
        Source::with_correlation(SourceAddr::Session(first_session), 1),
        MailId::NONE,
    );
    enqueue(
        &registry,
        TcpCapability::NAMESPACE,
        &unbind,
        Source::with_correlation(SourceAddr::Session(duplicate_session), 2),
        MailId::NONE,
    );

    let mut first_reply = None;
    let mut duplicate_reply = None;
    for _ in 0..2 {
        let event = rx.recv_timeout(Duration::from_secs(2)).expect("both unbind callers receive a reply");
        let EgressEvent::ToSession { session, kind_name, payload, .. } = event else {
            panic!("expected unbind reply to a session");
        };
        assert_eq!(kind_name, UnbindListenerResult::NAME);
        let reply = UnbindListenerResult::decode_from_bytes(&payload).expect("decode UnbindListenerResult");
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
            Some(UnbindListenerResult::Err { listener_name: ref name, ref reason })
                if name == &listener_name && reason == "unbind already in progress"
        ),
        "the duplicate caller receives the in-progress error: {duplicate_reply:?}",
    );
}

/// Tripwire: an outbound dial must correlate its parked reply to the
/// spawned cap-child session, and the connect-session lineage helper
/// must route `SessionWrite` to that actor rather than the accepted-
/// session grandchild path.
#[test]
#[allow(clippy::disallowed_methods)] // test-only loopback server thread; no actor lineage or runtime work.
fn connect_roundtrip_spawns_writable_session() {
    const CONSUMER: &str = "test.tcp.connect-consumer";
    const REPLY: &[u8] = b"loopback-reply";
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
    let addr = listener.local_addr().expect("loopback server address");
    let framed_reply = framed_body(REPLY);
    let server_thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept connect-side session");
        stream.write_all(&framed_reply).expect("write framed reply to connect-side session");
        stream.set_read_timeout(Some(Duration::from_secs(2))).expect("set server read timeout");
        let mut received = [0_u8; 17];
        stream.read_exact(&mut received).expect("connect-side SessionWrite reaches loopback server");
        received
    });

    let (registry, mailer, rx, _chassis) = boot_tcp_substrate();
    let consumer_rx = register_session_consumer(&registry, CONSUMER);
    let connect_reply = drive_and_decode::<Connect, ConnectResult>(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &Connect { addr: addr.to_string(), name: None, consumer: Some(mailbox_id_from_path(CONSUMER)) },
    );
    let (session_name, session_id, peer) = match connect_reply {
        ConnectResult::Ok { session_name, session_id, peer } => (session_name, session_id, peer),
        ConnectResult::Err { reason, .. } => panic!("connect failed: {reason}"),
    };
    assert!(!session_name.is_empty(), "connect result should name the spawned session");
    let session_path = format!("{}/{}:{session_name}", TcpCapability::NAMESPACE, TcpSessionActor::NAMESPACE);
    assert_eq!(
        mailbox_id_from_path(&session_path),
        session_id,
        "the documented MCP lineage path must fold to the spawned session id",
    );
    let peer = peer.parse::<SocketAddr>().expect("connect result peer is a socket address");
    assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST), "connect result peer should be on 127.0.0.1");

    let received = consumer_rx.recv_timeout(Duration::from_secs(2)).expect("connect consumer receives SessionData");
    let CapturedSessionMail::Data(received) = received else {
        panic!("expected SessionData, got {received:?}");
    };
    assert_eq!(received.session_name, session_name);
    assert_eq!(received.peer, peer.to_string());
    assert_eq!(received.bytes, REPLY);

    let sender_binding = NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x00C0_FFEE));
    NativeActorMailbox::<TcpCapability>::__new(
        registry.lookup(TcpCapability::NAMESPACE).expect("cap mailbox registered").0,
        &sender_binding,
    )
    .connect_session_write(&session_name, b"connect-roundtrip");
    sender_binding.flush_outbound();

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

    let (registry, mailer, rx, chassis) = boot_tcp_substrate();
    let session_alpha = SessionToken(Uuid::from_u128(0xA11A));
    let session_beta = SessionToken(Uuid::from_u128(0xB37A));
    let correlation_alpha = 0xA11A;
    let correlation_beta = 0xB37A;
    let root_alpha = MailId::new(MailboxId(0xA11A), correlation_alpha);
    let root_beta = MailId::new(MailboxId(0xB37A), correlation_beta);
    let settled_alpha = chassis.settlement_registry().subscribe_settlement(root_alpha);
    let settled_beta = chassis.settlement_registry().subscribe_settlement(root_beta);
    let cap_id = registry.lookup(TcpCapability::NAMESPACE).expect("cap mailbox registered");
    mailer.record_sent(root_alpha, root_alpha, None, root_alpha.sender, cap_id, Connect::ID);
    mailer.record_sent(root_beta, root_beta, None, root_beta.sender, cap_id, Connect::ID);
    enqueue(
        &registry,
        TcpCapability::NAMESPACE,
        &Connect { addr: addr_alpha.to_string(), name: Some("alpha".into()), consumer: None },
        Source::with_correlation(SourceAddr::Session(session_alpha), correlation_alpha),
        root_alpha,
    );
    enqueue(
        &registry,
        TcpCapability::NAMESPACE,
        &Connect { addr: addr_beta.to_string(), name: Some("beta".into()), consumer: None },
        Source::with_correlation(SourceAddr::Session(session_beta), correlation_beta),
        root_beta,
    );

    let mut saw_alpha = false;
    let mut saw_beta = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !(saw_alpha && saw_beta) {
        let event = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < deadline, "both connect replies arrive before the deadline");
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("connect-reply egress disconnected"),
        };
        let EgressEvent::ToSession { session, kind_name, payload, correlation_id, .. } = event else {
            assert!(Instant::now() < deadline, "both connect replies arrive before the deadline");
            continue;
        };
        if kind_name != ConnectResult::NAME {
            assert!(Instant::now() < deadline, "both connect replies arrive before the deadline");
            continue;
        }
        assert_eq!(kind_name, ConnectResult::NAME);
        let ConnectResult::Ok { session_name, peer, .. } =
            ConnectResult::decode_from_bytes(&payload).expect("decode concurrent connect reply")
        else {
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
    assert!(saw_alpha && saw_beta, "each caller receives its own reply");
    settled_alpha.recv_timeout(Duration::from_secs(2)).expect("alpha settles after its staged reply");
    settled_beta.recv_timeout(Duration::from_secs(2)).expect("beta settles after its staged reply");
    assert!(
        matches!(rx.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout)),
        "each staged connect emits exactly one result",
    );
    assert!(server_alpha.join().expect("alpha server thread completes").ip().is_loopback());
    assert!(server_beta.join().expect("beta server thread completes").ip().is_loopback());
}

/// Binding the same port twice fails the second bind. Uses
/// the first bind's actually-bound port to drive the second.
#[test]
fn bind_port_in_use_returns_err() {
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();

    let first: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("first".into()), consumer: None },
    );
    let local_port = match first {
        BindListenerResult::Ok { local_port, .. } => local_port,
        BindListenerResult::Err { reason, .. } => panic!("first bind failed: {reason}"),
    };

    // Second bind on the same port — must fail.
    let second: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: format!("127.0.0.1:{local_port}"), name: Some("second".into()), consumer: None },
    );
    match second {
        BindListenerResult::Ok { .. } => panic!("expected port-in-use Err"),
        BindListenerResult::Err { reason, addr } => {
            assert_eq!(addr, format!("127.0.0.1:{local_port}"));
            assert!(reason.starts_with("bind failed:"), "expected bind-fail reason, got: {reason}");
        }
    }
}

/// Unbind on an unknown name surfaces an Err with the name
/// echoed back.
#[test]
fn unbind_unknown_listener_errors() {
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();

    let reply: UnbindListenerResult =
        drive_and_decode(&registry, &rx, TcpCapability::NAMESPACE, &UnbindListener { listener_name: "nope".into() });
    match reply {
        UnbindListenerResult::Err { listener_name, .. } => {
            assert_eq!(listener_name, "nope");
        }
        UnbindListenerResult::Ok { .. } => panic!("expected Err for unknown listener"),
    }
}

/// A bound consumer receives one mail per complete frame even when a
/// frame body spans TCP writes, followed by a close notice on peer EOF.
#[test]
fn session_reassembles_frames_for_bound_consumer_and_reports_eof() {
    const CONSUMER: &str = "test.tcp.consumer";
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();
    let consumer_rx = register_session_consumer(&registry, CONSUMER);

    let bind: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener {
            addr: "127.0.0.1:0".into(),
            name: Some("delivery".into()),
            consumer: Some(mailbox_id_from_path(CONSUMER)),
        },
    );
    let local_port = match bind {
        BindListenerResult::Ok { local_port, .. } => local_port,
        BindListenerResult::Err { reason, .. } => panic!("bind failed: {reason}"),
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
    thread::sleep(Duration::from_millis(50));
    assert!(consumer_rx.try_recv().is_err(), "consumer must receive exactly two data mails and one close mail");
}

/// Tripwire: a consumer that is a *nested* actor still receives its
/// session mail. A loaded wasm component lives at the ADR-0099 lineage
/// path `aether.component/aether.embedded:<name>`, which is precisely
/// what the `consumer` field exists to serve — and precisely what a
/// runtime *name* cannot address, since `mailbox_id_from_name` refuses a
/// `/`-bearing path. Typing `consumer` as a `MailboxId` is what makes
/// this reachable; regressing it to a name would silently drop every
/// frame bound for a component.
#[test]
fn nested_lineage_consumer_receives_session_mail() {
    const CONSUMER: &str = "aether.component/aether.embedded:probe";
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();
    let consumer_rx = register_session_consumer(&registry, CONSUMER);

    let bind: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener {
            addr: "127.0.0.1:0".into(),
            name: Some("nested".into()),
            consumer: Some(mailbox_id_from_path(CONSUMER)),
        },
    );
    let local_port = match bind {
        BindListenerResult::Ok { local_port, .. } => local_port,
        BindListenerResult::Err { reason, .. } => panic!("bind failed: {reason}"),
    };

    let body = b"frame for a nested consumer";
    let mut client = TcpStream::connect(("127.0.0.1", local_port)).expect("connect loopback client");
    client.write_all(&framed_body(body)).expect("write one complete frame");

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
    const CONSUMER: &str = "test.tcp.rejection-consumer";
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();
    let consumer_rx = register_session_consumer(&registry, CONSUMER);

    let bind: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener {
            addr: "127.0.0.1:0".into(),
            name: Some("rejection".into()),
            consumer: Some(mailbox_id_from_path(CONSUMER)),
        },
    );
    let local_port = match bind {
        BindListenerResult::Ok { local_port, .. } => local_port,
        BindListenerResult::Err { reason, .. } => panic!("bind failed: {reason}"),
    };

    let mut client = TcpStream::connect(("127.0.0.1", local_port)).expect("connect loopback client");
    client.write_all(&u32::MAX.to_le_bytes()).expect("write oversize frame prefix");

    let closed = consumer_rx.recv_timeout(Duration::from_secs(2)).expect("SessionClosed arrives on frame rejection");
    let CapturedSessionMail::Closed(closed) = closed else {
        panic!("expected SessionClosed, got {closed:?}");
    };
    assert_eq!(closed.session_name, "conn-0");
    assert!(closed.peer.starts_with("127.0.0.1:"));
    assert!(closed.reason.starts_with("frame rejected: frame too large:"), "unexpected reason: {}", closed.reason);
    thread::sleep(Duration::from_millis(50));
    assert!(consumer_rx.try_recv().is_err(), "consumer must receive exactly one close mail");
}

/// Two concurrent binds on different ports both surface in
/// `ListListeners`.
#[test]
fn list_enumerates_two_concurrent_listeners() {
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();

    let _: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("admin".into()), consumer: None },
    );
    let _: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("game".into()), consumer: None },
    );

    let list: ListListenersResult =
        drive_and_decode(&registry, &rx, TcpCapability::NAMESPACE, &ListListeners::default());
    let mut names: Vec<String> = list.listeners.iter().map(|l| l.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["admin".to_string(), "game".to_string()]);
}
