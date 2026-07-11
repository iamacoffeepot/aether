//! Tests for the `aether.tcp` control plane: connect / bind / list / unbind
//! round-trips through a passive chassis with a real loopback socket.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{
    BindListener, BindListenerResult, Connect, ConnectResult, ListListeners, ListListenersResult, TcpCapability,
    TcpNativeExt, UnbindListener, UnbindListenerResult,
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
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
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
        &BindListener { addr: "127.0.0.1:0".into(), name: None },
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

/// Tripwire: an outbound dial must correlate its parked reply to the
/// spawned cap-child session, and the connect-session lineage helper
/// must route `SessionWrite` to that actor rather than the accepted-
/// session grandchild path.
#[test]
#[allow(clippy::disallowed_methods)] // test-only loopback server thread; no actor lineage or runtime work.
fn connect_roundtrip_spawns_writable_session() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
    let addr = listener.local_addr().expect("loopback server address");
    let server_thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept connect-side session");
        stream.set_read_timeout(Some(Duration::from_secs(2))).expect("set server read timeout");
        let mut received = [0_u8; 17];
        stream.read_exact(&mut received).expect("connect-side SessionWrite reaches loopback server");
        received
    });

    let (registry, mailer, rx, _chassis) = boot_tcp_substrate();
    let connect_reply = drive_and_decode::<Connect, ConnectResult>(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &Connect { addr: addr.to_string(), name: None },
    );
    let (session_name, session_address, session_id, peer) = match connect_reply {
        ConnectResult::Ok { session_name, session_address, session_id, peer } => {
            (session_name, session_address, session_id, peer)
        }
        ConnectResult::Err { reason, .. } => panic!("connect failed: {reason}"),
    };
    assert!(!session_name.is_empty(), "connect result should name the spawned session");
    assert_eq!(session_address, "aether.tcp/aether.tcp.session:conn-0");
    assert_eq!(mailbox_id_from_path(&session_address), session_id, "rendered session address folds to the spawned id");
    let peer = peer.parse::<SocketAddr>().expect("connect result peer is a socket address");
    assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST), "connect result peer should be on 127.0.0.1");

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

    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();
    let session_alpha = SessionToken(Uuid::from_u128(0xA11A));
    let session_beta = SessionToken(Uuid::from_u128(0xB37A));
    let correlation_alpha = 0xA11A;
    let correlation_beta = 0xB37A;
    enqueue(
        &registry,
        TcpCapability::NAMESPACE,
        &Connect { addr: addr_alpha.to_string(), name: Some("alpha".into()) },
        Source::with_correlation(SourceAddr::Session(session_alpha), correlation_alpha),
        MailId::new(MailboxId(0xA11A), correlation_alpha),
    );
    enqueue(
        &registry,
        TcpCapability::NAMESPACE,
        &Connect { addr: addr_beta.to_string(), name: Some("beta".into()) },
        Source::with_correlation(SourceAddr::Session(session_beta), correlation_beta),
        MailId::new(MailboxId(0xB37A), correlation_beta),
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
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("first".into()) },
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
        &BindListener { addr: format!("127.0.0.1:{local_port}"), name: Some("second".into()) },
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

// Pre-#775 the session round-trip test asserted that
// SessionData / SessionClosed broadcasts arrived at the egress
// after a real TCP client wrote then dropped. Issue 775 retired
// the BroadcastCapability + observation fan-out, so the
// session actor no longer publishes those kinds — the test was
// deleted with the broadcasts.

/// Two concurrent binds on different ports both surface in
/// `ListListeners`.
#[test]
fn list_enumerates_two_concurrent_listeners() {
    let (registry, _mailer, rx, _chassis) = boot_tcp_substrate();

    let _: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("admin".into()) },
    );
    let _: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("game".into()) },
    );

    let list: ListListenersResult =
        drive_and_decode(&registry, &rx, TcpCapability::NAMESPACE, &ListListeners::default());
    let mut names: Vec<String> = list.listeners.iter().map(|l| l.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["admin".to_string(), "game".to_string()]);
}
