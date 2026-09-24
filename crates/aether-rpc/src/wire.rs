//! `aether.rpc` wire vocabulary + the `Call` client primitive (issues
//! 750 / 763; folded back from the dissolved `aether-rpc` crate per
//! ADR-0124).
//!
//! Length-prefix frames carrying [`WireFrame`] bodies, layered
//! over the generic stream helpers in `aether-codec::frame` (ADR-0072).
//! The sibling [`server`](super::server) module's `RpcServerCapability`
//! speaks this wire over a TCP socket; the wire is intentionally
//! type-erased — endpoints are mail kinds, not request enums, so any new
//! mail kind both sides understand is reachable without a wire change.
//!
//! [`RpcClient`] is the outbound counterpart: it dials an RPC server,
//! runs the handshake, and frames inbound [`WireFrame`]s onto an mpsc.
//! It is native-only (it owns a `TcpStream` and an OS thread), gated at
//! the inline `client` module below so the wire vocabulary still compiles
//! for the wasm-header build.
//!
//! The full design (peer model, dispatch flow, settlement signalling) is
//! on issues 750 and 763.

use aether_data::{ActorPath, EngineId, KindId};
use serde::{Deserialize, Serialize};

/// Wire-format version negotiated at handshake. Bump on any breaking
/// shape change to [`WireFrame`] or its substructs; mismatched peers
/// get kicked (no downgrade, no negotiation per issue 750). Version 2
/// names a `Call`'s recipient by [`ActorPath`] and drops the address from
/// replies (issue 6570).
pub const WIRE_VERSION: u32 = 2;

/// One frame on the wire. Length-prefix-framed via
/// [`aether_codec::frame`]; wire-encoded body.
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
    /// 0..n per cid; the server emits one for every reply mail that
    /// comes back to it under the call's correlation.
    ReplyEvent {
        cid: u64,
        envelope: ReplyEnvelope,
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
    Substrate { engine_name: String, engine_version: String, kinds: Vec<KindDescriptor> },
    Client { client_name: String, client_version: String },
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

/// One `Call`'s mail on the wire: who it is for, its kind, and its
/// already-encoded bytes.
///
/// The recipient is a [`Recipient`], an engine selection plus an
/// [`ActorPath`]. The engine that hosts the recipient resolves the path
/// when the `Call` arrives (ADR-0230 §3); nothing upstream computes a
/// mailbox id for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailEnvelope {
    pub to: Recipient,
    pub kind: KindId,
    #[serde(with = "aether_data::bytes")]
    pub payload: Vec<u8>,
}

/// A `Call`'s recipient: which engine, and the actor's [`ActorPath`] in it.
///
/// `engine = None` names the local actor system of the server the `Call`
/// reached. `engine = Some(id)` asks a hub to relay the `Call` to that
/// engine's proxy, which sends it on with the path as written, so only
/// the hosting engine expands a short path. A path that does not resolve
/// there to a `Live` actor closes the call with [`RpcError::NotPresent`].
/// The path is valid by construction, so a malformed one fails the frame
/// decode and never reaches resolution.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Recipient {
    pub engine: Option<EngineId>,
    pub path: ActorPath,
}

impl Recipient {
    /// A recipient in the local actor system of the server the `Call`
    /// reaches (no engine routing).
    #[must_use]
    pub const fn local(path: ActorPath) -> Self {
        Self { engine: None, path }
    }
}

/// One reply mail on the wire: its kind and bytes, and no address. The
/// `ReplyEvent`'s `cid` already says which call it answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyEnvelope {
    pub kind: KindId,
    #[serde(with = "aether_data::bytes")]
    pub payload: Vec<u8>,
}

/// Reasons a `Call` can fail before the trace chain settles. v1 keeps
/// the variant set small — most failures (handler panics, decode
/// errors, etc.) surface as a `ReplyEvent` carrying a result kind
/// from the responder, not an `RpcError`.
///
/// It derives [`aether_data::Schema`] because the hub carries an engine's
/// refusal back in `aether.rpc.call_settled` unchanged, so a caller that
/// goes through the hub sees the same variant as one that dialed the
/// engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Schema)]
pub enum RpcError {
    /// The recipient's [`ActorPath`] does not resolve to a `Live` actor in
    /// the engine that hosts it, whatever the reason: never registered,
    /// still starting, dropped, or a short path that is ambiguous or names
    /// no declared child. `detail` is that engine's diagnostic, including
    /// ADR-0166's candidate spellings for an ambiguous hole.
    NotPresent { path: ActorPath, detail: String },
    /// The kind id isn't in this server's kind registry.
    UnknownKind { kind: KindId },
    /// Target carried `engine = Some(_)` — cross-engine routing is
    /// a phase-3 concern.
    UnsupportedTarget { reason: String },
    /// The peer announced a frame whose body exceeded the server's
    /// framing cap (`aether_codec::frame::max_frame_size`, see
    /// ADR-0072). Carries the announced `size` and the active `max` so
    /// the caller can decide how to react (build a release wasm, raise
    /// the frame-size config member, etc.) instead of seeing a
    /// bare `Connection reset by peer`. Widths are `u64` rather than
    /// `usize` so the wire encoding is stable across 32 / 64-bit peers.
    FrameTooLarge { size: u64, max: u64 },
    /// Catch-all for anything else (decode failures on the envelope
    /// payload, internal errors).
    Other { reason: String },
    /// Target carried `engine = Some(engine)`, and no proxy is registered
    /// for that engine on this hub: it was never spawned, has departed,
    /// or has not finished registering. Appended after every existing
    /// variant so their tags keep their positions.
    UnknownEngine { engine: EngineId },
}

#[cfg(not(target_family = "wasm"))]
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
#[cfg(not(target_family = "wasm"))]
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
    use std::time::Duration;

    /// How long [`RpcClient::connect`] waits for the server's `HelloAck`
    /// before failing with [`RpcClientError::Handshake`]. A healthy
    /// loopback handshake takes well under a millisecond, so ten seconds
    /// leaves four orders of magnitude of headroom for a busy scheduler.
    /// It is a third of the fleet's default 30-second proxy connect
    /// budget, so a silent peer fails a spawn inside that budget, and the
    /// worst case (a dial refused for most of the budget that then lands
    /// on a silent peer) stays under `FleetHarness`'s 60-second spawn cap.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
    ///   a `Bye`, or carried a mismatched `wire_version`, or no `HelloAck`
    ///   arrived within the handshake timeout.
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
        ///
        /// A peer that sends no `HelloAck` within ten seconds fails the
        /// connect with [`RpcClientError::Handshake`].
        pub fn connect(
            addr: &str,
            peer: PeerKind,
            on_frame: impl Fn() + Send + 'static,
        ) -> Result<RpcConnection, RpcClientError> {
            Self::connect_within(addr, peer, on_frame, HANDSHAKE_TIMEOUT)
        }

        /// [`RpcClient::connect`] with the handshake read bounded by
        /// `handshake_timeout` rather than [`HANDSHAKE_TIMEOUT`].
        fn connect_within(
            addr: &str,
            peer: PeerKind,
            on_frame: impl Fn() + Send + 'static,
            handshake_timeout: Duration,
        ) -> Result<RpcConnection, RpcClientError> {
            let stream = TcpStream::connect(addr).map_err(RpcClientError::Connect)?;
            stream.set_read_timeout(Some(handshake_timeout)).map_err(RpcClientError::Connect)?;
            let mut write_half = stream.try_clone().map_err(RpcClientError::Connect)?;
            let wake_handle = stream.try_clone().map_err(RpcClientError::Connect)?;

            // Handshake. Write Hello, then read exactly one frame and
            // require a HelloAck with a matching wire version. The
            // BufReader is created once over the original stream and moved
            // into the reader thread afterwards, so any bytes it buffered
            // past the HelloAck frame are not lost.
            write_frame(&mut write_half, &WireFrame::Hello(Hello { wire_version: WIRE_VERSION, peer }))
                .map_err(RpcClientError::Frame)?;

            let mut reader = BufReader::new(stream);
            let first: WireFrame = read_frame(&mut reader).map_err(|e| match e {
                FrameError::Io(io_err)
                    if matches!(io_err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                {
                    RpcClientError::Handshake(format!("no HelloAck within {} millis", handshake_timeout.as_millis()))
                }
                other => RpcClientError::Frame(other),
            })?;
            let server = match first {
                WireFrame::HelloAck(HelloAck { wire_version, server }) => {
                    if wire_version != WIRE_VERSION {
                        return Err(RpcClientError::Handshake(format!(
                            "wire_version mismatch: server={wire_version}, client={WIRE_VERSION}"
                        )));
                    }
                    server
                }
                WireFrame::Bye { reason } => {
                    return Err(RpcClientError::Handshake(format!("server rejected handshake: {reason}")));
                }
                other => {
                    return Err(RpcClientError::Handshake(format!("expected HelloAck, got {other:?}")));
                }
            };

            // The sidecar's reads must block without a deadline, or an idle connection reads as an error.
            reader.get_ref().set_read_timeout(None).map_err(RpcClientError::Connect)?;

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
                                    FrameError::Io(io_err) if io_err.kind() == io::ErrorKind::UnexpectedEof => {
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
                client: Self { write_half, next_cid: 1 },
                server,
                inbound: inbound_rx,
                reader: RpcReaderHandle { shutdown, wake_handle, thread: Some(thread) },
            })
        }

        /// Write a `Call` frame carrying `envelope`, returning the freshly
        /// minted `cid` the caller correlates replies against. The server
        /// answers with zero or more `ReplyEvent { cid }` frames followed
        /// by exactly one `ReplyEnd { cid }`.
        pub fn call(&mut self, envelope: MailEnvelope) -> Result<u64, RpcClientError> {
            let cid = self.next_cid;
            self.next_cid += 1;
            write_frame(&mut self.write_half, &WireFrame::Call { cid: Some(cid), envelope })
                .map_err(RpcClientError::Frame)?;
            Ok(cid)
        }

        /// Write a `Ping(nonce)` liveness probe. The server mirrors it
        /// back as `Pong(nonce)` on the inbound channel.
        pub fn ping(&mut self, nonce: u64) -> Result<(), RpcClientError> {
            write_frame(&mut self.write_half, &WireFrame::Ping(nonce)).map_err(RpcClientError::Frame)?;
            Ok(())
        }
    }

    #[cfg(test)]
    #[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
    mod tests {
        use super::{RpcClient, RpcClientError};
        use crate::{HelloAck, PeerKind, WIRE_VERSION, WireFrame};
        use aether_codec::frame::{read_frame, write_frame};
        use std::io::BufReader;
        use std::net::{TcpListener, TcpStream};
        use std::sync::mpsc;
        use std::thread;
        use std::thread::JoinHandle;
        use std::time::Duration;

        fn substrate_peer_kind() -> PeerKind {
            PeerKind::Substrate { engine_name: "test".into(), engine_version: "0.1.0".into(), kinds: vec![] }
        }

        fn client_peer_kind() -> PeerKind {
            PeerKind::Client { client_name: "rpc-client-test".into(), client_version: "0.0.1".into() }
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
                    &WireFrame::HelloAck(HelloAck { wire_version: WIRE_VERSION, server: substrate_peer_kind() }),
                )
                .expect("write HelloAck");
                // Return — drops the stream, closing the peer side.
            });

            let conn =
                RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {}).expect("client connects");
            server.join().expect("fake server thread");

            let frame = conn.inbound.recv_timeout(Duration::from_secs(2)).expect("Bye within 2s");
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
                    &WireFrame::HelloAck(HelloAck { wire_version: WIRE_VERSION + 1, server: substrate_peer_kind() }),
                )
                .expect("write HelloAck");
            });

            let result = RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {});
            server.join().expect("fake server thread");

            match result {
                Err(RpcClientError::Handshake(reason)) => {
                    assert!(reason.contains("wire_version"), "handshake error should mention wire_version: {reason}");
                }
                Err(other) => panic!("expected Handshake error, got {other:?}"),
                Ok(_) => panic!("mismatched wire_version should not yield a connection"),
            }
        }

        /// A peer that accepts TCP but never answers the handshake fails
        /// `connect` with `RpcClientError::Handshake` once the handshake
        /// timeout elapses, rather than blocking forever.
        #[test]
        fn silent_peer_fails_the_handshake_within_its_timeout() {
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let (port, server) = fake_server(move |_stream| {
                let _ = release_rx.recv();
            });

            let (result_tx, result_rx) = mpsc::channel();
            thread::spawn(move || {
                let result = RpcClient::connect_within(
                    &format!("127.0.0.1:{port}"),
                    client_peer_kind(),
                    || {},
                    Duration::from_millis(200),
                );
                let _ = result_tx.send(result);
            });

            match result_rx.recv_timeout(Duration::from_secs(5)).expect("connect returns within 5s") {
                Err(RpcClientError::Handshake(reason)) => {
                    assert!(reason.contains("HelloAck"), "handshake error should mention HelloAck: {reason}");
                }
                Err(other) => panic!("expected Handshake error, got {other:?}"),
                Ok(_) => panic!("a silent peer should not yield a connection"),
            }

            drop(release_tx);
            server.join().expect("fake server thread");
        }

        /// A connection that completes the handshake keeps no read
        /// timeout, so idling past the handshake timeout does not surface
        /// a synthetic `Bye` from the reader sidecar.
        #[test]
        fn completed_handshake_clears_the_read_timeout() {
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let (port, server) = fake_server(move |mut stream| {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let _hello: WireFrame = read_frame(&mut reader).expect("read Hello");
                write_frame(
                    &mut stream,
                    &WireFrame::HelloAck(HelloAck { wire_version: WIRE_VERSION, server: substrate_peer_kind() }),
                )
                .expect("write HelloAck");
                let _ = release_rx.recv();
            });

            let conn = RpcClient::connect_within(
                &format!("127.0.0.1:{port}"),
                client_peer_kind(),
                || {},
                Duration::from_millis(200),
            )
            .expect("client connects");

            match conn.inbound.recv_timeout(Duration::from_millis(800)) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                other => panic!("an idle connection should surface no frame, got {other:?}"),
            }

            drop(release_tx);
            server.join().expect("fake server thread");
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
