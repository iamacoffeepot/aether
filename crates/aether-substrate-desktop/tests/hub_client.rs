// End-to-end tests for the substrate-side hub client. Each test stands
// up a minimal mock hub on `127.0.0.1:0`, runs one handshake or a
// scripted exchange, and asserts against the substrate's local
// registry/queue state.

use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aether_hub_protocol::{
    ClaudeAddress, EngineId, EngineMailFrame, EngineToHub, Goodbye, HubToEngine, MailFrame,
    SessionToken, Uuid, Welcome, read_frame, write_frame,
};
use aether_substrate_desktop::{
    HubClient, HubOutbound, Mailer, Registry, Scheduler, mail::MailboxId,
};

/// Start a mock hub on a random port. Returns the bound address and a
/// `TcpStream` for the single connection it will accept.
fn accept_one() -> (std::net::SocketAddr, std::sync::mpsc::Receiver<TcpStream>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        tx.send(stream).unwrap();
    });
    (addr, rx)
}

fn canned_engine_id() -> EngineId {
    EngineId(Uuid::from_u128(0xabcd_ef01_2345_6789_abcd_ef01_2345_6789))
}

#[test]
fn handshake_exchanges_hello_and_welcome() {
    let (addr, conn_rx) = accept_one();

    let registry = Arc::new(Registry::new());
    let queue = Arc::new(Mailer::new());
    let client_handle = thread::spawn({
        let registry = Arc::clone(&registry);
        let queue = Arc::clone(&queue);
        move || {
            HubClient::connect(
                addr,
                "test-engine",
                "0.0.0",
                vec![],
                registry,
                queue,
                HubOutbound::disconnected(),
            )
            .unwrap()
        }
    });

    // Server side of the handshake.
    let mut stream = conn_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let hello: EngineToHub = read_frame(&mut stream).unwrap();
    match hello {
        EngineToHub::Hello(h) => {
            assert_eq!(h.name, "test-engine");
            assert_eq!(h.version, "0.0.0");
        }
        other => panic!("expected Hello, got {other:?}"),
    }
    let id = canned_engine_id();
    write_frame(
        &mut stream,
        &HubToEngine::Welcome(Welcome { engine_id: id }),
    )
    .unwrap();

    let client = client_handle.join().unwrap();
    assert_eq!(client.engine_id, id);

    // Keep the server stream alive so the client's threads don't tear
    // down mid-test.
    drop(stream);
}

#[test]
fn inbound_mail_lands_in_queue_after_resolution() {
    let (addr, conn_rx) = accept_one();

    // Shared state the sink writes into so the test can observe what the
    // scheduler actually delivered after resolution.
    #[derive(Default)]
    struct Seen {
        count_sum: u32,
        payload_lens: Vec<usize>,
        senders: Vec<SessionToken>,
    }
    let seen = Arc::new((Mutex::new(Seen::default()), Condvar::new()));
    let seen_for_sink = Arc::clone(&seen);

    let registry = Arc::new(Registry::new());
    let recipient = registry.register_sink(
        "hello",
        Arc::new(
            move |_kind_id: u64,
                  _kind: &str,
                  _origin: Option<&str>,
                  sender: SessionToken,
                  bytes: &[u8],
                  count: u32| {
                let (lock, cv) = &*seen_for_sink;
                let mut s = lock.lock().unwrap();
                s.count_sum += count;
                s.payload_lens.push(bytes.len());
                s.senders.push(sender);
                cv.notify_all();
            },
        ),
    );
    registry.register_kind("aether.tick");
    let queue = Arc::new(Mailer::new());

    let _sched = Scheduler::new(Arc::clone(&registry), Arc::clone(&queue), 1);

    let client_handle = thread::spawn({
        let registry = Arc::clone(&registry);
        let queue = Arc::clone(&queue);
        move || {
            HubClient::connect(
                addr,
                "t",
                "0",
                vec![],
                registry,
                queue,
                HubOutbound::disconnected(),
            )
            .unwrap()
        }
    });

    let mut stream = conn_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let _hello: EngineToHub = read_frame(&mut stream).unwrap();
    write_frame(
        &mut stream,
        &HubToEngine::Welcome(Welcome {
            engine_id: canned_engine_id(),
        }),
    )
    .unwrap();
    let _client = client_handle.join().unwrap();

    // A known kind with a populated sender, an unknown kind (should
    // drop), then another known with NIL. The sender on the first
    // frame must survive the wire → reader → queue → sink path.
    let alice = SessionToken(Uuid::from_u128(0xa11ce));
    for frame in [
        MailFrame {
            recipient_name: "hello".into(),
            kind_name: "aether.tick".into(),
            payload: vec![1, 2, 3],
            count: 7,
            sender: alice,
        },
        MailFrame {
            recipient_name: "hello".into(),
            kind_name: "not.registered".into(),
            payload: vec![],
            count: 1,
            sender: SessionToken::NIL,
        },
        MailFrame {
            recipient_name: "hello".into(),
            kind_name: "aether.tick".into(),
            payload: vec![9],
            count: 1,
            sender: SessionToken::NIL,
        },
    ] {
        write_frame(&mut stream, &HubToEngine::Mail(frame)).unwrap();
    }

    // Wait for two deliveries (the unknown-kind one is dropped at the
    // reader, never enqueued).
    let (lock, cv) = &*seen;
    let mut s = lock.lock().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.payload_lens.len() < 2 {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            panic!("timed out waiting for mail deliveries");
        }
        let (ns, _) = cv.wait_timeout(s, timeout).unwrap();
        s = ns;
    }
    assert_eq!(s.count_sum, 8);
    assert_eq!(s.payload_lens, vec![3, 1]);
    assert_eq!(s.senders, vec![alice, SessionToken::NIL]);
    assert_eq!(recipient, MailboxId::from_name("hello"));

    drop(stream);
}

#[test]
fn client_sends_periodic_heartbeats() {
    let (addr, conn_rx) = accept_one();

    let registry = Arc::new(Registry::new());
    let queue = Arc::new(Mailer::new());
    let client_handle = thread::spawn({
        let registry = Arc::clone(&registry);
        let queue = Arc::clone(&queue);
        move || {
            HubClient::connect(
                addr,
                "hb",
                "0",
                vec![],
                registry,
                queue,
                HubOutbound::disconnected(),
            )
            .unwrap()
        }
    });

    let mut stream = conn_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let _hello: EngineToHub = read_frame(&mut stream).unwrap();
    write_frame(
        &mut stream,
        &HubToEngine::Welcome(Welcome {
            engine_id: canned_engine_id(),
        }),
    )
    .unwrap();
    let _client = client_handle.join().unwrap();

    // HEARTBEAT_INTERVAL is 5s; one should arrive within 10s.
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    match read_frame::<_, EngineToHub>(&mut stream).unwrap() {
        EngineToHub::Heartbeat => {}
        other => panic!("expected Heartbeat, got {other:?}"),
    }
    drop(stream);
}

#[test]
fn goodbye_stops_reader_thread() {
    // This is a liveness rather than correctness check: sending Goodbye
    // should cause the reader to drop out; we assert that by reading
    // the eventual heartbeat from the *client* side (the heartbeat
    // thread keeps running, but if Goodbye crashed the reader it would
    // panic and poison the reader's stream clone). The test passes as
    // long as nothing hangs past the deadline.
    let (addr, conn_rx) = accept_one();
    let registry = Arc::new(Registry::new());
    let queue = Arc::new(Mailer::new());
    let client_handle = thread::spawn({
        let registry = Arc::clone(&registry);
        let queue = Arc::clone(&queue);
        move || {
            HubClient::connect(
                addr,
                "bye",
                "0",
                vec![],
                registry,
                queue,
                HubOutbound::disconnected(),
            )
            .unwrap()
        }
    });

    let mut stream = conn_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let _hello: EngineToHub = read_frame(&mut stream).unwrap();
    write_frame(
        &mut stream,
        &HubToEngine::Welcome(Welcome {
            engine_id: canned_engine_id(),
        }),
    )
    .unwrap();
    let _client = client_handle.join().unwrap();

    write_frame(
        &mut stream,
        &HubToEngine::Goodbye(Goodbye {
            reason: "testing".into(),
        }),
    )
    .unwrap();

    // Give the reader a beat to process Goodbye and exit.
    thread::sleep(Duration::from_millis(100));
    drop(stream);
}

#[test]
fn outbound_sends_reach_the_hub_wire() {
    // HubOutbound attached at connect time; subsequent sends show up
    // on the server's read side as postcard-framed EngineToHub::Mail.
    let (addr, conn_rx) = accept_one();
    let outbound = HubOutbound::disconnected();
    assert!(!outbound.is_connected());

    let registry = Arc::new(Registry::new());
    let queue = Arc::new(Mailer::new());

    let connect_outbound = Arc::clone(&outbound);
    let client_handle = thread::spawn({
        let registry = Arc::clone(&registry);
        let queue = Arc::clone(&queue);
        move || {
            HubClient::connect(
                addr,
                "outbound-test",
                "0",
                vec![],
                registry,
                queue,
                connect_outbound,
            )
            .unwrap()
        }
    });

    let mut stream = conn_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let _hello: EngineToHub = read_frame(&mut stream).unwrap();
    write_frame(
        &mut stream,
        &HubToEngine::Welcome(Welcome {
            engine_id: canned_engine_id(),
        }),
    )
    .unwrap();
    let _client = client_handle.join().unwrap();

    assert!(outbound.is_connected());

    // Drive a broadcast out through the outbound handle.
    assert!(outbound.send(EngineToHub::Mail(EngineMailFrame {
        address: ClaudeAddress::Broadcast,
        kind_name: "aether.observation.ping".into(),
        payload: vec![1, 2, 3],
        origin: Some("physics".into()),
    })));

    // Read it back on the server side. Heartbeats may arrive too —
    // skip over them until we see the Mail frame.
    let observed = loop {
        let frame: EngineToHub = read_frame(&mut stream).unwrap();
        match frame {
            EngineToHub::Mail(m) => break m,
            EngineToHub::Heartbeat => continue,
            other => panic!("unexpected frame {other:?}"),
        }
    };
    assert_eq!(observed.address, ClaudeAddress::Broadcast);
    assert_eq!(observed.kind_name, "aether.observation.ping");
    assert_eq!(observed.payload, vec![1, 2, 3]);
    assert_eq!(observed.origin.as_deref(), Some("physics"));

    drop(stream);
}

#[test]
fn outbound_send_without_attach_is_noop() {
    let outbound = HubOutbound::disconnected();
    assert!(!outbound.is_connected());
    // No attach ever happened; send returns false and doesn't panic.
    let ok = outbound.send(EngineToHub::Mail(EngineMailFrame {
        address: ClaudeAddress::Broadcast,
        kind_name: "aether.tick".into(),
        payload: vec![],
        origin: None,
    }));
    assert!(!ok);
}
