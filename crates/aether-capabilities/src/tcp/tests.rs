//! Tests for the `aether.tcp` control plane: bind / list / unbind
//! round-trips through a passive chassis with a real loopback socket.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{
    BindListener, BindListenerResult, ListListeners, ListListenersResult, TcpCapability, UnbindListener,
    UnbindListenerResult,
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

/// Binding the same port twice fails the second bind. Uses
/// the first bind's actually-bound port to drive the second.
#[test]
fn bind_port_in_use_returns_err() {
    let (registry, rx, _chassis) = boot_tcp_substrate();

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
    let (registry, rx, _chassis) = boot_tcp_substrate();

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
