//! Tests for the `aether.tcp` control plane: bind / list / unbind
//! round-trips through a passive chassis with a real loopback socket.

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{
    BindListener, BindListenerResult, ListListeners, ListListenersResult, SessionClosed, SessionData, TcpCapability,
    UnbindListener, UnbindListenerResult,
};
use aether_actor::Addressable;
use aether_data::{Kind, SessionToken, Uuid};
use aether_kinds::descriptors;
use aether_kinds::trace::Nanos;
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
fn boot_tcp_substrate() -> (Arc<Registry>, mpsc::Receiver<EgressEvent>, PassiveChassis<TestChassis>) {
    let (registry, mailer, rx) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TcpCapability>(())
        .build_passive()
        .expect("TcpCapability boots");
    (registry, rx, chassis)
}

fn session_reply() -> Source {
    Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xfeed))))
}

#[derive(Debug)]
enum CapturedSessionMail {
    Data(SessionData),
    Closed(SessionClosed),
}

fn register_session_consumer(registry: &Registry, name: &str) -> mpsc::Receiver<CapturedSessionMail> {
    let (tx, rx) = mpsc::channel();
    registry
        .try_register_inbox(
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
    let id = registry.lookup(cap_namespace).expect("cap mailbox registered");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("cap entry") else {
        panic!("expected mailbox entry");
    };
    let bytes = mail.encode_into_bytes();
    handler.enqueue(OwnedDispatch::disarmed(
        K::ID,
        K::NAME.to_owned(),
        None,
        session_reply(),
        MailRef::from(bytes),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        aether_data::MailboxId(0),
    ));

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
    let (registry, rx, _chassis) = boot_tcp_substrate();

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

/// Binding the same port twice fails the second bind. Uses
/// the first bind's actually-bound port to drive the second.
#[test]
fn bind_port_in_use_returns_err() {
    let (registry, rx, _chassis) = boot_tcp_substrate();

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
    let (registry, rx, _chassis) = boot_tcp_substrate();

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
    let (registry, rx, _chassis) = boot_tcp_substrate();
    let consumer_rx = register_session_consumer(&registry, CONSUMER);

    let bind: BindListenerResult = drive_and_decode(
        &registry,
        &rx,
        TcpCapability::NAMESPACE,
        &BindListener { addr: "127.0.0.1:0".into(), name: Some("delivery".into()), consumer: Some(CONSUMER.into()) },
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

/// Two concurrent binds on different ports both surface in
/// `ListListeners`.
#[test]
fn list_enumerates_two_concurrent_listeners() {
    let (registry, rx, _chassis) = boot_tcp_substrate();

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
