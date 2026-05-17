//! `aether.rpc` client — the outbound counterpart to
//! [`RpcServerCapability`](crate::rpc::server::RpcServerCapability)
//! (issue 763 P1).
//!
//! [`RpcClient`] is a plain struct, not an actor. It dials an RPC
//! server, runs the `Hello` / `HelloAck` handshake, and spawns a
//! reader sidecar thread that frames inbound [`WireFrame`]s onto an
//! mpsc. It is deliberately actor-agnostic: `aether-mcp` (a plain
//! binary with no mailbox or `Mailer`) is a consumer too, so readiness
//! notification is a generic `on_frame` closure rather than a wake-mail
//! address. Actor-based consumers — the per-engine proxy in issue 763
//! P3 — capture their `Mailer` + mailbox + wake kind in the closure and
//! fire wake mail; non-actor consumers pass `|| {}` and block / poll
//! [`RpcConnection::inbound`] directly.
//!
//! The whole module is native-only (gated at the `mod` declaration in
//! `rpc/mod.rs`) — it owns a `TcpStream` and an OS thread.
//!
//! See issue 763 for the forward-model architecture this is the first
//! piece of.

use crate::rpc::wire::{Hello, HelloAck, MailEnvelope, PeerKind, WIRE_VERSION, WireFrame};
use aether_codec::frame::{FrameError, read_frame, write_frame};
use std::fmt;
use std::io::{self, BufReader};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

/// The outbound half of a live RPC connection: the write socket plus a
/// monotonic call-id counter. `.call()` and `.ping()` write frames;
/// inbound frames arrive on [`RpcConnection::inbound`] via the reader
/// sidecar.
pub struct RpcClient {
    write_half: TcpStream,
    next_cid: u64,
}

/// Everything [`RpcClient::connect`] hands back: the outbound `client`,
/// the `server` identity lifted from `HelloAck`, the `inbound` frame
/// channel, and the `reader` sidecar handle (dropping it tears the
/// connection down).
pub struct RpcConnection {
    /// Outbound half — `.call()` / `.ping()`.
    pub client: RpcClient,
    /// The server's `HelloAck` identity. For a `PeerKind::Substrate`
    /// server this carries the kind manifest the per-engine proxy
    /// (issue 763 P3) caches at connect time.
    pub server: PeerKind,
    /// Inbound frames from the reader sidecar. Actor consumers drain
    /// this from their `on_frame` wake handler; non-actor consumers
    /// `recv()` it directly.
    pub inbound: mpsc::Receiver<WireFrame>,
    /// Reader sidecar handle. Dropping it flags shutdown, shuts the
    /// socket to wake the blocked read, and joins the thread.
    pub reader: RpcReaderHandle,
}

/// Handle to the reader sidecar thread. `Drop` is orderly teardown:
/// flag shutdown, `shutdown(Both)` the socket to wake the blocked
/// `read_frame`, then join.
pub struct RpcReaderHandle {
    shutdown: Arc<AtomicBool>,
    /// A clone of the connection's stream, kept solely so `Drop` can
    /// `shutdown()` it and wake the reader thread's blocked read.
    wake_handle: TcpStream,
    thread: Option<JoinHandle<()>>,
}

impl Drop for RpcReaderHandle {
    fn drop(&mut self) {
        // Order matters: set the flag first so the reader sees it the
        // moment the shutdown wakes its blocked read.
        self.shutdown.store(true, Ordering::Release);
        let _ = self.wake_handle.shutdown(Shutdown::Both);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Failure modes for [`RpcClient::connect`] and the frame-writing
/// methods.
///
/// - `Connect` — the TCP dial (or the reader-thread spawn) failed.
/// - `Handshake` — the server's first frame wasn't a `HelloAck`, was a
///   `Bye`, or carried a mismatched `wire_version`.
/// - `Frame` — a codec error reading or writing a frame.
#[derive(Debug)]
pub enum RpcClientError {
    Connect(io::Error),
    Handshake(String),
    Frame(FrameError),
}

impl fmt::Display for RpcClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "rpc connect: {e}"),
            Self::Handshake(reason) => write!(f, "rpc handshake: {reason}"),
            Self::Frame(e) => write!(f, "rpc frame: {e}"),
        }
    }
}

impl std::error::Error for RpcClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(e) => Some(e),
            Self::Frame(e) => Some(e),
            Self::Handshake(_) => None,
        }
    }
}

impl RpcClient {
    /// Dial `addr`, run the `Hello` / `HelloAck` handshake identifying
    /// as `peer`, and spawn the reader sidecar.
    ///
    /// `on_frame` is the scheduling kick: the reader calls it after
    /// pushing each frame onto the inbound channel (and once more after
    /// the final synthetic `Bye` on EOF / error). Actor consumers
    /// capture their `Mailer` + mailbox + wake kind in the closure and
    /// fire wake mail; non-actor consumers pass `|| {}` and block /
    /// poll [`RpcConnection::inbound`] directly.
    pub fn connect(
        addr: &str,
        peer: PeerKind,
        on_frame: impl Fn() + Send + 'static,
    ) -> Result<RpcConnection, RpcClientError> {
        let stream = TcpStream::connect(addr).map_err(RpcClientError::Connect)?;
        let mut write_half = stream.try_clone().map_err(RpcClientError::Connect)?;
        let wake_handle = stream.try_clone().map_err(RpcClientError::Connect)?;

        // Handshake. Write Hello, then read exactly one frame and
        // require a HelloAck with a matching wire version. The
        // BufReader is created once over the original stream and moved
        // into the reader thread afterwards, so any bytes it buffered
        // past the HelloAck frame are not lost.
        write_frame(
            &mut write_half,
            &WireFrame::Hello(Hello {
                wire_version: WIRE_VERSION,
                peer,
            }),
        )
        .map_err(RpcClientError::Frame)?;

        let mut reader = BufReader::new(stream);
        let first: WireFrame = read_frame(&mut reader).map_err(RpcClientError::Frame)?;
        let server = match first {
            WireFrame::HelloAck(HelloAck {
                wire_version,
                server,
            }) => {
                if wire_version != WIRE_VERSION {
                    return Err(RpcClientError::Handshake(format!(
                        "wire_version mismatch: server={wire_version}, client={WIRE_VERSION}"
                    )));
                }
                server
            }
            WireFrame::Bye { reason } => {
                return Err(RpcClientError::Handshake(format!(
                    "server rejected handshake: {reason}"
                )));
            }
            other => {
                return Err(RpcClientError::Handshake(format!(
                    "expected HelloAck, got {other:?}"
                )));
            }
        };

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);
        let (inbound_tx, inbound_rx) = mpsc::channel::<WireFrame>();

        let thread = std::thread::Builder::new()
            .name("aether-rpc-client-reader".into())
            .spawn(move || {
                loop {
                    if shutdown_for_thread.load(Ordering::Acquire) {
                        break;
                    }
                    let frame: WireFrame = match read_frame(&mut reader) {
                        Ok(f) => f,
                        Err(e) => {
                            // Consumer-initiated teardown: the
                            // RpcReaderHandle's Drop flips the flag and
                            // shuts the socket, surfacing here as a
                            // read error. No synthetic Bye — nobody is
                            // reading the channel.
                            if shutdown_for_thread.load(Ordering::Acquire) {
                                break;
                            }
                            // Peer-initiated close (EOF) or a real read
                            // error: surface it as a Bye so the
                            // consumer's drain observes the close.
                            let reason = match &e {
                                FrameError::Io(io_err)
                                    if io_err.kind() == io::ErrorKind::UnexpectedEof =>
                                {
                                    "eof".to_string()
                                }
                                other => format!("read error: {other}"),
                            };
                            let _ = inbound_tx.send(WireFrame::Bye { reason });
                            on_frame();
                            break;
                        }
                    };
                    if inbound_tx.send(frame).is_err() {
                        // Receiver dropped — the consumer is gone.
                        break;
                    }
                    on_frame();
                }
            })
            .map_err(RpcClientError::Connect)?;

        Ok(RpcConnection {
            client: Self {
                write_half,
                next_cid: 1,
            },
            server,
            inbound: inbound_rx,
            reader: RpcReaderHandle {
                shutdown,
                wake_handle,
                thread: Some(thread),
            },
        })
    }

    /// Write a `Call` frame carrying `envelope`, returning the freshly
    /// minted `cid` the caller correlates replies against. The server
    /// answers with zero or more `ReplyEvent { cid }` frames followed
    /// by exactly one `ReplyEnd { cid }`.
    pub fn call(&mut self, envelope: MailEnvelope) -> Result<u64, RpcClientError> {
        let cid = self.next_cid;
        self.next_cid += 1;
        write_frame(
            &mut self.write_half,
            &WireFrame::Call {
                cid: Some(cid),
                envelope,
            },
        )
        .map_err(RpcClientError::Frame)?;
        Ok(cid)
    }

    /// Write a `Ping(nonce)` liveness probe. The server mirrors it back
    /// as `Pong(nonce)` on the inbound channel.
    pub fn ping(&mut self, nonce: u64) -> Result<(), RpcClientError> {
        write_frame(&mut self.write_half, &WireFrame::Ping(nonce))
            .map_err(RpcClientError::Frame)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{RpcClient, RpcClientError};
    use crate::rpc::server::{RpcServerCapability, RpcServerConfig, RpcServerHandle};
    use crate::rpc::test_echo::{TestEchoActor, TestEchoReply, TestEchoRequest};
    use crate::rpc::wire::{
        HelloAck, MailEnvelope, MailboxAddress, PeerKind, WIRE_VERSION, WireFrame,
    };
    use crate::test_chassis::{TestChassis, fresh_substrate};
    use crate::trace::TraceObserverCapability;
    use aether_actor::Actor;
    use aether_codec::frame::{read_frame, write_frame};
    use aether_data::{Kind, mailbox_id_from_name};
    use aether_substrate::chassis::builder::Builder;
    use std::io::BufReader;
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    fn substrate_peer_kind() -> PeerKind {
        PeerKind::Substrate {
            engine_name: "test".into(),
            engine_version: "0.1.0".into(),
            kinds: vec![],
        }
    }

    fn client_peer_kind() -> PeerKind {
        PeerKind::Client {
            client_name: "rpc-client-test".into(),
            client_version: "0.0.1".into(),
        }
    }

    /// Spin a one-shot fake server on an OS-picked port: bind, hand the
    /// port back, and on a background thread accept exactly one
    /// connection and run `handle` against it. Used by the error-path
    /// tests that need a server behaving in ways the real
    /// `RpcServerCapability` never would (bad wire version, immediate
    /// close). Returns the port + the server thread's join handle.
    fn fake_server(handle: impl FnOnce(TcpStream) + Send + 'static) -> (u16, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let port = listener.local_addr().expect("local_addr").port();
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("fake server accept");
            handle(stream);
        });
        (port, thread)
    }

    /// Full socket round-trip: boot `RpcServerCapability` + the echo
    /// actor + `TraceObserverCapability`, connect a real `RpcClient`,
    /// fire a `Call` carrying a `TestEchoRequest`, and drain the inbound
    /// channel — expect `ReplyEvent { TestEchoReply }` then
    /// `ReplyEnd { Ok }`. Issue 750's tests dispatched in-process; this
    /// is the first that exercises the actual TCP path end to end.
    #[test]
    fn call_echo_round_trips_over_the_socket() {
        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            // TraceObserver fires `Settled { root }` once a dispatched
            // chain drains; without it RpcServer's settlement
            // subscription never wakes and no `ReplyEnd` is written.
            .with_actor::<TraceObserverCapability>(())
            .with_actor::<TestEchoActor>(())
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: substrate_peer_kind(),
            })
            .build_passive()
            .expect("caps boot");

        let port = chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;

        // No on_frame work needed — `recv_timeout` returning is the
        // observable signal we care about. iamacoffeepot/aether#835:
        // a prior version asserted `frames_seen >= 2` against an
        // AtomicUsize bumped inside the hook, but the hook is a
        // post-enqueue scheduling kick by design (see `connect`'s
        // doc) — the test thread can wake from `recv_timeout` before
        // the reader thread reaches `on_frame()`, racing the
        // assertion. End-to-end correctness here is the two
        // `recv_timeout` returns below: ReplyEvent then ReplyEnd.
        let mut conn = RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {})
            .expect("client connects + handshakes");

        // The handshake handed back the server's identity.
        match &conn.server {
            PeerKind::Substrate { engine_name, .. } => assert_eq!(engine_name, "test"),
            PeerKind::Client { .. } => panic!("expected Substrate peer kind from server"),
        }

        let echo_payload = postcard::to_allocvec(&TestEchoRequest { value: 42 })
            .expect("test setup: TestEchoRequest serializes via postcard");
        let echo_mailbox = mailbox_id_from_name(<TestEchoActor as Actor>::NAMESPACE);
        let cid = conn
            .client
            .call(MailEnvelope {
                to: MailboxAddress::local(echo_mailbox),
                from: None,
                kind: <TestEchoRequest as Kind>::ID,
                correlation_id: None,
                payload: echo_payload,
            })
            .expect("call writes");

        // First frame back: ReplyEvent carrying the echoed reply.
        // recv_timeout so a hung settlement fails the test instead of
        // blocking forever.
        let event = conn
            .inbound
            .recv_timeout(Duration::from_secs(5))
            .expect("ReplyEvent within 5s");
        let envelope = match event {
            WireFrame::ReplyEvent {
                cid: ev_cid,
                envelope,
            } => {
                assert_eq!(ev_cid, cid);
                envelope
            }
            other => panic!("expected ReplyEvent, got {other:?}"),
        };
        assert_eq!(envelope.kind, <TestEchoReply as Kind>::ID);
        let decoded: TestEchoReply = postcard::from_bytes(&envelope.payload).expect("decode reply");
        assert_eq!(decoded.value, 42);

        // Then ReplyEnd closes the call.
        let end = conn
            .inbound
            .recv_timeout(Duration::from_secs(5))
            .expect("ReplyEnd within 5s");
        match end {
            WireFrame::ReplyEnd {
                cid: end_cid,
                result,
            } => {
                assert_eq!(end_cid, cid);
                result.expect("ReplyEnd result Ok");
            }
            other => panic!("expected ReplyEnd, got {other:?}"),
        }
    }

    /// `Ping(nonce)` round-trips as `Pong(nonce)` over the socket.
    #[test]
    fn ping_pongs_over_the_socket() {
        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: substrate_peer_kind(),
            })
            .build_passive()
            .expect("rpc server boots");

        let port = chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;

        let mut conn = RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {})
            .expect("client connects");

        conn.client.ping(0x00c0_ffee).expect("ping writes");
        let pong = conn
            .inbound
            .recv_timeout(Duration::from_secs(2))
            .expect("Pong within 2s");
        assert_eq!(pong, WireFrame::Pong(0x00c0_ffee));
    }

    /// A peer that completes the handshake then closes surfaces as a
    /// synthetic `Bye { reason: "eof" }` on the inbound channel — the
    /// reader sidecar's EOF path.
    #[test]
    fn peer_close_surfaces_eof_bye_on_inbound() {
        let (port, server) = fake_server(|mut stream| {
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let _hello: WireFrame = read_frame(&mut reader).expect("read Hello");
            write_frame(
                &mut stream,
                &WireFrame::HelloAck(HelloAck {
                    wire_version: WIRE_VERSION,
                    server: substrate_peer_kind(),
                }),
            )
            .expect("write HelloAck");
            // Return — drops the stream, closing the peer side.
        });

        let conn = RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {})
            .expect("client connects");
        server.join().expect("fake server thread");

        let frame = conn
            .inbound
            .recv_timeout(Duration::from_secs(2))
            .expect("Bye within 2s");
        match frame {
            WireFrame::Bye { reason } => assert_eq!(reason, "eof"),
            other => panic!("expected Bye, got {other:?}"),
        }
    }

    /// A server that answers with a mismatched `wire_version` is
    /// rejected at connect time as `RpcClientError::Handshake`, not a
    /// silent hang.
    #[test]
    fn wire_version_mismatch_surfaces_as_handshake_error() {
        let (port, server) = fake_server(|mut stream| {
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let _hello: WireFrame = read_frame(&mut reader).expect("read Hello");
            write_frame(
                &mut stream,
                &WireFrame::HelloAck(HelloAck {
                    wire_version: WIRE_VERSION + 1,
                    server: substrate_peer_kind(),
                }),
            )
            .expect("write HelloAck");
        });

        let result = RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {});
        server.join().expect("fake server thread");

        match result {
            Err(RpcClientError::Handshake(reason)) => assert!(
                reason.contains("wire_version"),
                "handshake error should mention wire_version: {reason}",
            ),
            Err(other) => panic!("expected Handshake error, got {other:?}"),
            Ok(_) => panic!("mismatched wire_version should not yield a connection"),
        }
    }

    /// Dialing a closed port is an `RpcClientError::Connect`. Bind an
    /// OS-picked port, drop the listener, then dial it — the port is
    /// free for the microseconds between drop and connect.
    #[test]
    fn connect_to_closed_port_is_connect_error() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        match RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {}) {
            Err(RpcClientError::Connect(_)) => {}
            Err(other) => panic!("expected Connect error, got {other:?}"),
            Ok(_) => panic!("dialing a closed port should not yield a connection"),
        }
    }
}
