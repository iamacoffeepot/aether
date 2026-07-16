//! The ADR-0149 Demo, cross-process: boot the `bloomery` bin against a temp
//! database, seal a synthetic single-workpiece bloom through `aether.store.*`
//! typed mail over RPC, `kill -9` the process, restart it against the same
//! database file, and prove journal replay + outbox republish converge to the
//! sealed state — and that a second overlapping seal loses cleanly.
//!
//! This dials the bin over raw `WireFrame::Call` frames, the `FleetBench` pattern
//! (a real process, a real socket, a real SIGKILL) — the process-boundary
//! complement to the in-process reopen test in `src/store/tests.rs`.

#![allow(clippy::unwrap_used)]

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery_host::store::{
    AppendEvent, AppendEventResult, ClaimSeal, ClaimSealResult, DrainOutbox, DrainOutboxResult, EnqueueOutbox,
    EnqueueOutboxResult, ReplayJournal, ReplayJournalResult,
};
use aether_capabilities::rpc::{Hello, HelloAck, MailEnvelope, MailboxAddress, PeerKind, WIRE_VERSION, WireFrame};
use aether_codec::frame::{read_frame, write_frame};
use aether_data::{Kind, mailbox_id_from_path};
use serde::Serialize;

/// Reserve a free localhost port by binding `:0`, then release it for the bin to
/// claim. A small race window, tolerated by the connect retry loop.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Fork the `bloomery` bin against `db` on `port`.
fn spawn(port: u16, db: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_bloomery"))
        .env("AETHER_RPC_PORT", port.to_string())
        .env("AETHER_STORE_PATH", db)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Connect to the bin, retrying until it has bound its RPC port.
fn connect(port: u16) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => {
                stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
                return stream;
            }
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Err(error) => panic!("could not reach the bloomery bin on port {port}: {error}"),
        }
    }
}

/// Handshake as a client peer.
fn handshake(stream: &mut TcpStream) {
    let hello = WireFrame::Hello(Hello {
        wire_version: WIRE_VERSION,
        peer: PeerKind::Client { client_name: "recovery-test".into(), client_version: "0.0.1".into() },
    });
    write_frame(stream, &hello).unwrap();
    match read_frame(stream).unwrap() {
        WireFrame::HelloAck(HelloAck { wire_version, .. }) => assert_eq!(wire_version, WIRE_VERSION),
        other => panic!("expected HelloAck, got {other:?}"),
    }
}

/// Issue one typed `Call` to the `aether.store` mailbox and decode the reply of
/// the expected kind (collected from the `ReplyEvent` stream, closed by
/// `ReplyEnd`).
fn call<Req, Reply>(stream: &mut TcpStream, cid: u64, request: &Req) -> Reply
where
    Req: Kind + Serialize,
    Reply: Kind,
{
    let frame = WireFrame::Call {
        cid: Some(cid),
        envelope: MailEnvelope {
            to: MailboxAddress { engine: None, mailbox: mailbox_id_from_path("aether.store") },
            from: None,
            kind: Req::ID,
            correlation_id: None,
            payload: request.encode_into_bytes(),
        },
    };
    write_frame(stream, &frame).unwrap();

    let mut reply: Option<Reply> = None;
    loop {
        match read_frame(stream).unwrap() {
            WireFrame::ReplyEvent { cid: got, envelope } => {
                assert_eq!(got, cid, "ReplyEvent cid mismatch");
                if envelope.kind == Reply::ID {
                    reply = Reply::decode_from_bytes(&envelope.payload);
                }
            }
            WireFrame::ReplyEnd { cid: got, result } => {
                assert_eq!(got, cid, "ReplyEnd cid mismatch");
                result.unwrap();
                return reply.expect("a reply of the expected kind arrived before ReplyEnd");
            }
            other => panic!("unexpected frame for call {cid}: {other:?}"),
        }
    }
}

/// Terminate a child with SIGKILL (`Child::kill` is SIGKILL on unix) and reap it.
fn kill9(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn kill_and_restart_converges_over_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    // First boot: seal a synthetic single-workpiece bloom through typed mail.
    let port = free_port();
    let child = spawn(port, db);
    let mut stream = connect(port);
    handshake(&mut stream);

    let append: AppendEventResult =
        call(&mut stream, 1, &AppendEvent { idempotency_key: "seal-1".into(), event: b"seal-event".to_vec() });
    assert_eq!(append, AppendEventResult::Applied { sequence: 1 });

    let seal: ClaimSealResult = call(&mut stream, 2, &ClaimSeal { bloom: vec![1; 32], members: vec!["wp".into()] });
    assert_eq!(seal, ClaimSealResult::Sealed);

    let enqueued: EnqueueOutboxResult =
        call(&mut stream, 3, &EnqueueOutbox { topic: "landing_receipt".into(), payload: b"receipt".to_vec() });
    assert_eq!(enqueued, EnqueueOutboxResult::Ok { sequence: 1 });

    // Crash: SIGKILL mid-service, after the committed transactions.
    drop(stream);
    kill9(child);

    // Restart against the same database file.
    let port = free_port();
    let child = spawn(port, db);
    let mut stream = connect(port);
    handshake(&mut stream);

    // Journal replay: the sealed event survived the crash.
    let replay: ReplayJournalResult = call(&mut stream, 1, &ReplayJournal);
    match replay {
        ReplayJournalResult::Ok { records } => {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].idempotency_key, "seal-1");
            assert_eq!(records[0].event, b"seal-event");
        }
        ReplayJournalResult::Err { error } => panic!("journal replay failed: {error}"),
    }

    // The membership survived: a second overlapping seal loses cleanly.
    let seal_again: ClaimSealResult =
        call(&mut stream, 2, &ClaimSeal { bloom: vec![2; 32], members: vec!["wp".into()] });
    assert_eq!(seal_again, ClaimSealResult::Conflict { workpiece: "wp".into() });

    // Outbox republish: the undelivered landing receipt is still drainable.
    let drained: DrainOutboxResult = call(&mut stream, 3, &DrainOutbox);
    match drained {
        DrainOutboxResult::Ok { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].topic, "landing_receipt");
            assert_eq!(entries[0].payload, b"receipt");
        }
        DrainOutboxResult::Err { error } => panic!("outbox drain failed: {error}"),
    }

    drop(stream);
    kill9(child);
}
