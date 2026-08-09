//! The ADR-0149 Demo, cross-process: boot the `bloomery` bin against a temp
//! database, seal a synthetic single-workpiece bloom through `aether.store.*`
//! typed mail over RPC, `kill -9` the process, restart it against the same
//! database file, and prove journal replay + outbox republish converge to the
//! sealed state — and that a second overlapping seal loses cleanly.
//!
//! This dials the bin over raw `WireFrame::Call` frames, the `FleetHarness` pattern
//! (a real process, a real socket, a real SIGKILL) — the process-boundary
//! complement to the in-process reopen test in `src/store/tests.rs`.

#![allow(clippy::unwrap_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "cross-process wire fixtures address root caps by their rendered runtime name — the RPC Call surface under test"
)]

mod common;

use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{BloomId, Digest, Event, Fact, IdempotencyKey};
use aether_chassis_bloomery::store::{
    AppendEvent, AppendEventResult, ClaimSeal, ClaimSealResult, DrainOutbox, DrainOutboxResult, EnqueueOutbox,
    EnqueueOutboxResult, ReplayJournal, ReplayJournalResult,
};
use aether_codec::frame::{read_frame, write_frame};
use aether_data::wire::to_vec;
use aether_data::{Kind, mailbox_id_from_path};
use aether_rpc::{Hello, HelloAck, MailEnvelope, MailboxAddress, PeerKind, WIRE_VERSION, WireFrame};
use common::{Coordinator, free_port};
use serde::Serialize;

/// Fork the `bloomery` bin against `db` on `port`, reaped when the returned
/// guard drops.
fn spawn(port: u16, db: &str) -> Coordinator {
    Coordinator::spawn(port, &[("AETHER_STORE_PATH", db)])
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

#[test]
fn kill_and_restart_converges_over_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    // First boot: seal a synthetic single-workpiece bloom through typed mail.
    let port = free_port();
    let coordinator = spawn(port, db);
    let mut stream = connect(port);
    handshake(&mut stream);

    // A real, wire-encoded bloom-protocol event — the shape the host journals.
    // The control core replays this journal at boot and decodes each record as an
    // `Event`; a non-`Event` record trips its fail-fast boot-replay abort
    // (ADR-0063), so the synthetic seed must be a valid encoded event, not opaque
    // bytes. `Fact::Land` on an orphan bloom reduces to a clean rejection, so the
    // replay rebuilds without incident.
    let event = Event {
        idempotency_key: IdempotencyKey("seal-1".to_owned()),
        fact: Fact::Land { bloom: BloomId(Digest::from_bytes([7; 32])), new_head: Digest::from_bytes([9; 32]) },
    };
    let event_bytes = to_vec(&event).unwrap();

    let append: AppendEventResult =
        call(&mut stream, 1, &AppendEvent { idempotency_key: "seal-1".into(), event: event_bytes.clone() });
    assert_eq!(append, AppendEventResult::Applied { sequence: 1 });

    let seal: ClaimSealResult = call(&mut stream, 2, &ClaimSeal { bloom: vec![1; 32], members: vec!["wp".into()] });
    assert_eq!(seal, ClaimSealResult::Sealed);

    let enqueued: EnqueueOutboxResult =
        call(&mut stream, 3, &EnqueueOutbox { topic: "landing_receipt".into(), payload: b"receipt".to_vec() });
    assert_eq!(enqueued, EnqueueOutboxResult::Ok { sequence: 1 });

    // Crash: SIGKILL mid-service, after the committed transactions.
    drop(stream);
    coordinator.kill9();

    // Restart against the same database file.
    let port = free_port();
    let _coordinator = spawn(port, db);
    let mut stream = connect(port);
    handshake(&mut stream);

    // Journal replay: the sealed event survived the crash.
    let replay: ReplayJournalResult = call(&mut stream, 1, &ReplayJournal);
    match replay {
        ReplayJournalResult::Ok { records } => {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].idempotency_key, "seal-1");
            assert_eq!(records[0].event, event_bytes);
        }
        ReplayJournalResult::Err { error } => panic!("journal replay failed: {error}"),
    }

    // The membership survived: a second overlapping seal loses cleanly.
    let seal_again: ClaimSealResult =
        call(&mut stream, 2, &ClaimSeal { bloom: vec![2; 32], members: vec!["wp".into()] });
    assert_eq!(seal_again, ClaimSealResult::Conflict { workpiece: "wp".into() });

    // Outbox republish: the undelivered landing receipt is still drainable.
    let drained: DrainOutboxResult = call(&mut stream, 3, &DrainOutbox { topic: None });
    match drained {
        DrainOutboxResult::Ok { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].topic, "landing_receipt");
            assert_eq!(entries[0].payload, b"receipt");
        }
        DrainOutboxResult::Err { error } => panic!("outbox drain failed: {error}"),
    }
}
