//! A raw-frame RPC client for the cross-process tests: connect, handshake, and
//! issue one typed `Call`.
//!
//! Shared rather than re-derived per test binary because there is exactly one
//! way to talk to the coordinator's RPC ingress, and three hand-rolled copies of
//! it drift — the reply-collection loop in particular, whose "keep the last
//! envelope of the expected kind, return it at `ReplyEnd`" shape is subtle
//! enough that a copy which got it slightly wrong would fail intermittently and
//! look like a coordinator bug. The spawn-and-handshake transaction lives here
//! for the same reason: a fork retry that only one suite owns leaves every
//! other caller burning the handshake deadline against a child that never came
//! up.

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot reach its process reports it by panicking")]

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use aether_codec::frame::{read_frame, write_frame};
use aether_data::{Kind, MailboxId};
use aether_rpc::{Hello, HelloAck, MailEnvelope, MailboxAddress, PeerKind, WIRE_VERSION, WireFrame};
use serde::Serialize;

use super::process::{Coordinator, Ingress};

/// How long one Hello probe may wait. A coordinator that has bound its listener
/// but not yet reached the mail loop completes TCP (kernel listen backlog) and
/// then says nothing; at the 20s call timeout that is enough to burn the
/// handshake deadline in one attempt (#5116).
const HELLO_PROBE: Duration = Duration::from_secs(1);

/// Timeout restored on a stream that has completed Hello, so later `Call`
/// reads have a real budget.
const CALL_READ_TIMEOUT: Duration = Duration::from_secs(20);

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
    connect_and_hello(port, client_name, CALL_READ_TIMEOUT)
}

fn connect_and_hello(port: u16, client_name: &str, timeout: Duration) -> Result<TcpStream, String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|error| format!("connecting: {error}"))?;
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream.set_write_timeout(Some(timeout)).unwrap();
    try_handshake(&mut stream, client_name)?;
    stream.set_read_timeout(Some(CALL_READ_TIMEOUT)).unwrap();
    stream.set_write_timeout(None).unwrap();
    Ok(stream)
}

/// Connect and handshake as one retried unit against an already-chosen port.
///
/// Retrying the pair rather than the connect alone is what makes this safe
/// against a peer that completes TCP before it can answer the wire: a
/// connect-only retry hands back a socket whose Hello nobody has read.
///
/// This helper cannot see the child. If the process that was supposed to bind
/// `port` has already exited, the loop burns its deadline against a socket that
/// will never accept — the full-suite flake at this panic. It is for a peer the
/// caller did not fork (an in-process chassis whose bound port it already
/// holds); a caller that owns the fork uses [`spawn_and_connect`], which dials
/// the port the child itself announced.
///
/// # Panics
/// No coordinator answered a handshake on `port` inside the deadline.
#[must_use]
pub fn connect_and_handshake(port: u16, client_name: &str) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(30);
    match handshake_while_alive(port, client_name, deadline, || true) {
        Ok(stream) => stream,
        Err(refusal) => panic!("no coordinator answered a handshake on port {port}: {refusal}"),
    }
}

/// Run `spawn` and handshake as one transaction: a fresh child per attempt,
/// against the port that child says it bound, the whole fork retried after an
/// early exit. Returns the live guard beside the stream so the caller cannot
/// keep a connection to a stranger.
///
/// `spawn` forks with RPC port `0`, so the child holds a port from the moment
/// it binds — there is no reserve-then-release window for a sibling to take it,
/// and the port this helper dials came from the child's own boot log rather
/// than from a guess about who owns it.
///
/// # Panics
/// No child completed a handshake inside `budget`.
pub fn spawn_and_connect(
    client_name: &str,
    budget: Duration,
    mut spawn: impl FnMut() -> Coordinator,
) -> (Coordinator, TcpStream) {
    let deadline = Instant::now() + budget;
    let mut last = String::from("no attempt");
    while Instant::now() < deadline {
        let mut coordinator = spawn();
        match handshake_our_child(&mut coordinator, client_name, deadline) {
            Ok(stream) => return (coordinator, stream),
            Err(why) => last = why,
        }
    }
    panic!("no coordinator answered a handshake: {last}");
}

/// Handshake the port this child announced, and only that one.
///
/// The child names its own RPC port as it binds, so there is no candidate set
/// to probe and no way to reach a stranger: a port the coordinator never
/// claimed is never dialled. A child that dies instead of announcing is
/// abandoned at once, which is what lets the caller retry the whole fork
/// instead of burning the handshake deadline (#5116).
fn handshake_our_child(
    coordinator: &mut Coordinator,
    client_name: &str,
    deadline: Instant,
) -> Result<TcpStream, String> {
    let port = coordinator.await_port(Ingress::Rpc, deadline)?;

    handshake_while_alive(port, client_name, deadline, || coordinator.is_alive())
}

/// Poll the one-attempt handshake while `alive` stays true. An exited child
/// is abandoned immediately — it will never become ready — so the caller can
/// retry the whole spawn instead of burning the handshake deadline against a
/// port that lost the bind race (#5116).
pub fn handshake_while_alive(
    port: u16,
    client_name: &str,
    deadline: Instant,
    mut alive: impl FnMut() -> bool,
) -> Result<TcpStream, String> {
    let mut last = String::from("child exited before a handshake attempt");
    while alive() && Instant::now() < deadline {
        match connect_and_hello(port, client_name, HELLO_PROBE) {
            Ok(stream) if alive() => return Ok(stream),
            Ok(_) => return Err(format!("child on port {port} exited after handshake")),
            Err(why) => {
                last = why;
                if !alive() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    if alive() {
        Err(last)
    } else {
        Err(format!("child on port {port} exited: {last}"))
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
