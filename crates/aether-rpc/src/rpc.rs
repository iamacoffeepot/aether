//! `aether.rpc` wire vocabulary + the `Call` client primitive (issues
//! 750 / 763, extracted to `aether-rpc` per ADR-0102).
//!
//! Length-prefix postcard frames carrying [`WireFrame`] bodies, layered
//! over the generic stream helpers in `aether-codec::frame` (ADR-0072).
//! `RpcServerCapability` (in `aether-capabilities`) speaks this wire over
//! a TCP socket; the wire is intentionally type-erased — endpoints are
//! mail kinds, not request enums, so any new mail kind both sides
//! understand is reachable without a wire change.
//!
//! [`RpcClient`] is the outbound counterpart: it dials an RPC server,
//! runs the handshake, and frames inbound [`WireFrame`]s onto an mpsc.
//! It is native-only (it owns a `TcpStream` and an OS thread), gated at
//! the inline `client` module below so the wire vocabulary still compiles
//! for the wasm-header build.
//!
//! The full design (peer model, dispatch flow, settlement signalling) is
//! on issues 750 and 763.

use aether_data::{EngineId, KindId, MailboxId};
use serde::{Deserialize, Serialize};

/// Wire-format version negotiated at handshake. Bump on any breaking
/// shape change to [`WireFrame`] or its substructs; mismatched peers
/// get kicked (no downgrade, no negotiation in v1 per issue 750).
pub const WIRE_VERSION: u32 = 1;

/// One frame on the wire. Length-prefix-framed via
/// [`aether_codec::frame`]; postcard-encoded body.
///
/// `cid` correlates a `Call` to its replies. `Call { cid: None }` is
/// fire-and-forget; `Call { cid: Some(n) }` expects zero or more
/// `ReplyEvent { cid: n, .. }` frames followed by exactly one
/// `ReplyEnd { cid: n, .. }` frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireFrame {
    Hello(Hello),
    HelloAck(HelloAck),
    /// Caller-to-server dispatch request. `cid = None` skips reply
    /// tracking entirely; `cid = Some(n)` opens an in-flight entry the
    /// server closes with `ReplyEnd { cid: n, .. }`.
    Call {
        cid: Option<u64>,
        envelope: MailEnvelope,
    },
    /// One reply mail observed in the trace chain of `cid`'s call.
    /// 0..n per cid; the server emits one for every mail addressed
    /// back at the `RpcServer` mailbox with `correlation_id = cid`.
    ReplyEvent {
        cid: u64,
        envelope: MailEnvelope,
    },
    /// Settlement notice for `cid` — the trace root of the original
    /// `Call` has settled (per ADR-0080). Exactly one per cid. After
    /// this frame the server discards all state for `cid` and ignores
    /// any further mail addressed with that correlation id.
    ReplyEnd {
        cid: u64,
        result: Result<(), RpcError>,
    },
    /// Liveness probe. Caller sends a `Ping(token)`; peer mirrors as
    /// `Pong(token)`. Token is opaque — typically a monotonic counter
    /// for round-trip-time measurement.
    Ping(u64),
    Pong(u64),
    /// Graceful shutdown notice. The sender will close the connection
    /// after writing this frame; the receiver drops its in-flight
    /// state for the connection. Not required — TCP close is also a
    /// valid shutdown — but lets the peer log a structured reason.
    Bye {
        reason: String,
    },
}

/// First frame sent by either side on a fresh connection. The server
/// replies with [`HelloAck`]; mismatched `wire_version` kicks the
/// connection (no downgrade).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub wire_version: u32,
    pub peer: PeerKind,
}

/// Server's response to [`Hello`]. Mirrors the wire version (so the
/// caller can confirm the server agrees) and identifies the server's
/// own peer kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    pub wire_version: u32,
    pub server: PeerKind,
}

/// Who's on the other end of a connection.
///
/// - `Substrate` peers (chassis hosting actors) declare their engine
///   identity + kind vocabulary so callers know which kinds the engine
///   can dispatch. `kinds` is intentionally shallow for v1 — fuller
///   schema rides in a future `describe_kinds` RPC kind rather than
///   bloating every handshake.
/// - `Client` peers (CLI / TUI / external) just identify themselves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerKind {
    Substrate {
        engine_name: String,
        engine_version: String,
        kinds: Vec<KindDescriptor>,
    },
    Client {
        client_name: String,
        client_version: String,
    },
}

/// Minimal kind-vocabulary entry carried in [`PeerKind::Substrate`].
/// V1 carries id + name only; structural detail (handler list, schema
/// shape) lives behind a `describe_kinds` RPC kind rather than the
/// handshake so the handshake stays cheap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindDescriptor {
    pub id: KindId,
    pub name: String,
}

/// One mail envelope on the wire.
///
/// `to` is the destination — `engine = None` means "this server's
/// local actor system". The hub later cross-routes `engine = Some(_)`
/// envelopes to the named substrate; for v1 the server rejects
/// non-local targets with [`RpcError::UnsupportedTarget`].
///
/// `from` is `Some` when the originator wants replies (mail back at
/// the `RpcServer` with `correlation_id = cid` round-trips to this
/// peer); `None` is fire-and-forget at the envelope layer regardless
/// of whether the outer `Call.cid` is set.
///
/// `correlation_id` is the mail-system correlation that responders
/// use to `ctx.reply()` against. `RpcServer` sets this to the outer
/// `Call.cid` on dispatch so any actor in the trace chain that
/// replies routes back to the originating peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailEnvelope {
    pub to: MailboxAddress,
    pub from: Option<MailboxAddress>,
    pub kind: KindId,
    pub correlation_id: Option<u64>,
    pub payload: Vec<u8>,
}

/// Engine-aware mailbox address. `engine = None` resolves against the
/// local actor system; `engine = Some(_)` is the hub-routing case
/// (parked for v1, see [`RpcError::UnsupportedTarget`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MailboxAddress {
    pub engine: Option<EngineId>,
    pub mailbox: MailboxId,
}

impl MailboxAddress {
    /// Address a local mailbox (no engine routing).
    #[must_use]
    pub const fn local(mailbox: MailboxId) -> Self {
        Self {
            engine: None,
            mailbox,
        }
    }
}

/// Reasons a `Call` can fail before the trace chain settles. v1 keeps
/// the variant set small — most failures (handler panics, decode
/// errors, etc.) surface as a `ReplyEvent` carrying a result kind
/// from the responder, not an `RpcError`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RpcError {
    /// The target mailbox isn't registered in this server's local
    /// actor system.
    UnknownMailbox { mailbox: MailboxId },
    /// The kind id isn't in this server's kind registry.
    UnknownKind { kind: KindId },
    /// Target carried `engine = Some(_)` — cross-engine routing is
    /// a phase-3 concern.
    UnsupportedTarget { reason: String },
    /// The peer announced a frame whose body exceeded the server's
    /// framing cap (`aether_codec::frame::max_frame_size`, see
    /// ADR-0072). Carries the announced `size` and the active `max` so
    /// the caller can decide how to react (build a release wasm, raise
    /// the cap via `AETHER_MAX_FRAME_SIZE`, etc.) instead of seeing a
    /// bare `Connection reset by peer`. Widths are `u64` rather than
    /// `usize` so the wire encoding is stable across 32 / 64-bit peers.
    FrameTooLarge { size: u64, max: u64 },
    /// Catch-all for anything else (decode failures on the envelope
    /// payload, internal errors).
    Other { reason: String },
}

#[cfg(not(target_arch = "wasm32"))]
pub use client::{RpcClient, RpcClientError, RpcConnection, RpcReaderHandle};

/// `aether.rpc` client — the outbound counterpart to the
/// `RpcServerCapability` server (issue 763 P1).
///
/// [`RpcClient`] is a plain struct, not an actor. It dials an RPC
/// server, runs the `Hello` / `HelloAck` handshake, and spawns a
/// reader sidecar thread that frames inbound [`WireFrame`]s onto an
/// mpsc. It is deliberately actor-agnostic: `aether-mcp` (a plain
/// binary with no mailbox or `Mailer`) is a consumer too, so readiness
/// notification is a generic `on_frame` closure rather than a wake-mail
/// address.
///
/// The whole module is native-only — it owns a `TcpStream` and an OS
/// thread, so it is gated off the wasm-header build.
#[cfg(not(target_arch = "wasm32"))]
mod client {
    use super::{Hello, HelloAck, MailEnvelope, PeerKind, WIRE_VERSION, WireFrame};
    use aether_codec::frame::{FrameError, read_frame, write_frame};
    use std::error;
    use std::fmt;
    use std::io::{self, BufReader};
    use std::net::{Shutdown, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::thread::JoinHandle;

    /// The outbound half of a live RPC connection: the write socket plus
    /// a monotonic call-id counter. `.call()` and `.ping()` write frames;
    /// inbound frames arrive on [`RpcConnection::inbound`] via the reader
    /// sidecar.
    pub struct RpcClient {
        write_half: TcpStream,
        next_cid: u64,
    }

    /// Everything [`RpcClient::connect`] hands back: the outbound
    /// `client`, the `server` identity lifted from `HelloAck`, the
    /// `inbound` frame channel, and the `reader` sidecar handle (dropping
    /// it tears the connection down).
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
    /// - `Handshake` — the server's first frame wasn't a `HelloAck`, was
    ///   a `Bye`, or carried a mismatched `wire_version`.
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

    impl error::Error for RpcClientError {
        fn source(&self) -> Option<&(dyn error::Error + 'static)> {
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
        /// pushing each frame onto the inbound channel (and once more
        /// after the final synthetic `Bye` on EOF / error). Actor
        /// consumers capture their `Mailer` + mailbox + wake kind in the
        /// closure and fire wake mail; non-actor consumers pass `|| {}`
        /// and block / poll [`RpcConnection::inbound`] directly.
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

            // Transport thread below the mail layer — it carries inbound mail in;
            // no inbound chain to inherit, so no settlement umbrella to honor.
            #[allow(clippy::disallowed_methods)]
            let thread = thread::Builder::new()
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

        /// Write a `Ping(nonce)` liveness probe. The server mirrors it
        /// back as `Pong(nonce)` on the inbound channel.
        pub fn ping(&mut self, nonce: u64) -> Result<(), RpcClientError> {
            write_frame(&mut self.write_half, &WireFrame::Ping(nonce))
                .map_err(RpcClientError::Frame)?;
            Ok(())
        }
    }

    #[cfg(test)]
    #[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
    mod tests {
        use super::{RpcClient, RpcClientError};
        use crate::rpc::{HelloAck, PeerKind, WIRE_VERSION, WireFrame};
        use aether_codec::frame::{read_frame, write_frame};
        use std::io::BufReader;
        use std::net::{TcpListener, TcpStream};
        use std::thread;
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

        /// Spin a one-shot fake server on an OS-picked port: bind, hand
        /// the port back, and on a background thread accept exactly one
        /// connection and run `handle` against it. Used by the error-path
        /// tests that need a server behaving in ways the real
        /// `RpcServerCapability` never would (bad wire version, immediate
        /// close). Returns the port + the server thread's join handle.
        fn fake_server(handle: impl FnOnce(TcpStream) + Send + 'static) -> (u16, JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
            let port = listener.local_addr().expect("local_addr").port();
            let thread = thread::spawn(move || {
                let (stream, _) = listener.accept().expect("fake server accept");
                handle(stream);
            });
            (port, thread)
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

            let result =
                RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {});
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
}
