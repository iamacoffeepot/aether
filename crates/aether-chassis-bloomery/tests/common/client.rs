//! A raw-frame RPC client for the cross-process tests: connect, handshake, and
//! issue one typed `Call`.
//!
//! Shared rather than re-derived per test binary because there is exactly one
//! way to talk to the coordinator's RPC ingress, and three hand-rolled copies of
//! it drift — the reply-collection loop in particular, whose "keep the last
//! envelope of the expected kind, return it at `ReplyEnd`" shape is subtle
//! enough that a copy which got it slightly wrong would fail intermittently and
//! look like a coordinator bug.

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot reach its process reports it by panicking")]

use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use aether_codec::frame::{read_frame, write_frame};
use aether_data::{Kind, MailboxId};
use aether_rpc::{Hello, HelloAck, MailEnvelope, MailboxAddress, PeerKind, WIRE_VERSION, WireFrame};
use serde::Serialize;

/// Handshake as a client peer named `client_name`, reporting a refusal instead
/// of panicking — the fallible half [`try_connect_and_handshake`] returns.
fn try_handshake(stream: &mut TcpStream, client_name: &str) -> Result<(), String> {
    let hello = WireFrame::Hello(Hello {
        wire_version: WIRE_VERSION,
        peer: PeerKind::Client { client_name: client_name.into(), client_version: "0.0.1".into() },
    });
    write_frame(stream, &hello).map_err(|error| format!("writing Hello: {error}"))?;
    match read_frame(stream).map_err(|error| format!("reading HelloAck: {error}"))? {
        WireFrame::HelloAck(HelloAck { wire_version, .. }) if wire_version == WIRE_VERSION => Ok(()),
        other => Err(format!("expected a matching HelloAck, got {other:?}")),
    }
}

/// Connect and handshake once, reporting the refusal instead of retrying it.
///
/// Connect failures begin with `connecting:`. Handshake failures name the
/// `Hello` / `HelloAck` step that refused, so a caller can tell a closed port
/// from a stranger that answered TCP but not the wire.
pub fn try_connect_and_handshake(port: u16, client_name: &str) -> Result<TcpStream, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| format!("connecting: {error}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    try_handshake(&mut stream, client_name)?;
    Ok(stream)
}

/// Connect and handshake as one retried unit.
///
/// Retrying the pair rather than the connect alone is what makes this safe under
/// a suite that forks many coordinators at once. A port is reserved by binding
/// `:0` and releasing it, so between the release and the bin's own bind another
/// process can take it — and the loser then *connects successfully* to a
/// stranger and fails at the handshake, which a connect-only retry cannot
/// recover from.
///
/// # Panics
/// No coordinator answered a handshake on `port` inside the deadline.
pub fn connect_and_handshake(port: u16, client_name: &str) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match try_connect_and_handshake(port, client_name) {
            Ok(stream) => return stream,
            Err(refusal) => {
                assert!(Instant::now() < deadline, "no coordinator answered a handshake on port {port}: {refusal}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// The `Call` frame `request` makes when addressed at `mailbox` under `cid`.
pub fn call_frame<Req: Kind + Serialize>(cid: u64, mailbox: MailboxId, request: &Req) -> WireFrame {
    WireFrame::Call {
        cid: Some(cid),
        envelope: MailEnvelope {
            to: MailboxAddress { engine: None, mailbox },
            from: None,
            kind: Req::ID,
            correlation_id: None,
            payload: request.encode_into_bytes(),
        },
    }
}

/// Collect `cid`'s reply stream up to its `ReplyEnd`, decoding the last envelope
/// whose kind is `Reply`.
///
/// # Panics
/// The stream faulted, carried a foreign cid, ended in an error, or closed
/// without a reply of the expected kind.
pub fn await_reply<Reply: Kind>(stream: &mut TcpStream, cid: u64) -> Reply {
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

/// Issue one typed `Call` to `mailbox` and decode its reply.
///
/// # Panics
/// As [`await_reply`], plus a write that faulted.
pub fn call<Req, Reply>(stream: &mut TcpStream, cid: u64, mailbox: MailboxId, request: &Req) -> Reply
where
    Req: Kind + Serialize,
    Reply: Kind,
{
    write_frame(stream, &call_frame(cid, mailbox, request)).unwrap();
    await_reply(stream, cid)
}
