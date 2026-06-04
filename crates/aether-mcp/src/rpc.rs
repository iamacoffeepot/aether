//! `RpcSession` — the `aether-mcp` side of one outbound RPC connection
//! to the hub (issue 763 P5b).
//!
//! Wraps [`RpcClient`] / [`RpcConnection`] (issue 763 P1) with the
//! demultiplexing the MCP tool surface needs: many tool calls share
//! the one socket, so a router thread drains the connection's single
//! inbound channel and fans each `ReplyEvent` / `ReplyEnd` to the
//! [`RpcSession::call`] future waiting on that `cid`.
//!
//! The hub is restartable (ADR-0089, iamacoffeepot/aether#1212): when it
//! dies the reader sidecar surfaces a synthetic `WireFrame::Bye`, the
//! router drains `pending`, and every in-flight `call()` sees its
//! channel close. Rather than surface that as a permanent error,
//! `RpcSession` re-dials the stashed `hub_addr` and retries the call
//! once. The per-connection state lives in a swappable [`Connection`]
//! cell behind the shared `Arc<RpcSession>`, so the handle and the
//! `call*(&self)` signatures stay stable across a re-dial.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aether_capabilities::rpc::{
    MailEnvelope, PeerKind, RpcClient, RpcClientError, RpcConnection, RpcReaderHandle, WireFrame,
};
use aether_codec::frame::FrameError;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::task;
use tokio::time;

/// How many times a self-healing `call()` re-dials a dead hub before
/// surfacing the error. One re-dial covers the common "hub bounced"
/// case; the bounded count keeps a still-down hub from busy-spinning.
const RECONNECT_ATTEMPTS: u32 = 3;

/// Backoff between re-dial attempts. Short enough that a restarted hub
/// is picked up promptly, bounded so a tool call against a genuinely
/// down hub returns a clean error in a couple of seconds.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(250);

type Pending = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<WireFrame>>>>;

/// All the state bound to one live socket. Held behind a cell inside
/// [`RpcSession`] so a re-dial can swap the whole bundle atomically:
/// dropping the prior `Connection` runs `RpcReaderHandle::Drop` (tears
/// the dead socket down) and ends the old router thread once its
/// `inbound` channel closes.
struct Connection {
    /// Write half. Behind a `std::sync::Mutex` so concurrent tool
    /// calls serialize their `Call` writes — and so a write + the
    /// matching `pending` registration happen under one lock section,
    /// closing the race against the router thread.
    client: Mutex<RpcClient>,
    /// `cid` → the channel feeding the `call()` future awaiting it.
    /// The router thread routes inbound frames here; `call()` registers
    /// an entry before its write is visible and clears it on `ReplyEnd`.
    pending: Pending,
    /// Set once the router thread exits (peer `Bye`, or `inbound`
    /// closing). After this, the router will never deliver another
    /// reply or drain `pending`, so a `call()` that registered too
    /// late would hang forever — it checks this flag under the
    /// `pending` lock and bails to a transport error instead, prompting
    /// a re-dial. Toggled by the router while holding the `pending`
    /// lock, so the check-and-register in `call_once` is atomic against
    /// the router's death.
    dead: Arc<AtomicBool>,
    /// The hub's `HelloAck` identity, cached at connect time.
    server: PeerKind,
    /// Keeps the connection's reader sidecar alive — its `Drop` tears
    /// the socket down. Declared before `_router` so it drops first:
    /// ending the reader closes `inbound`, which ends the router
    /// thread's blocking `recv()`.
    _reader: RpcReaderHandle,
    /// The demux thread. Detached on drop — it exits on its own once
    /// `_reader`'s teardown closes `inbound`.
    _router: JoinHandle<()>,
}

impl Connection {
    /// Dial `addr`, run the `Hello` / `HelloAck` handshake, and spawn
    /// the demux router thread. Blocking — call it off the async
    /// runtime.
    fn establish(addr: &str) -> anyhow::Result<Self> {
        let RpcConnection {
            client,
            server,
            inbound,
            reader,
        } = RpcClient::connect(
            addr,
            PeerKind::Client {
                client_name: "aether-mcp".to_owned(),
                client_version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            // Non-actor consumer: the router thread blocks on
            // `inbound.recv()` directly, so the wake hook is a no-op.
            || {},
        )?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(AtomicBool::new(false));
        let router_pending = Arc::clone(&pending);
        let router_dead = Arc::clone(&dead);
        // Out-of-process aether-mcp router — not an engine actor; there is no
        // settlement/trace umbrella in this process to opt out of.
        #[allow(clippy::disallowed_methods)]
        let router = thread::Builder::new()
            .name("aether-mcp-rpc-router".into())
            .spawn(move || {
                // `inbound.recv()` ends with `Err` once the reader
                // sidecar drops its sender (connection torn down).
                while let Ok(frame) = inbound.recv() {
                    let cid = match &frame {
                        WireFrame::ReplyEvent { cid, .. } | WireFrame::ReplyEnd { cid, .. } => *cid,
                        WireFrame::Bye { reason } => {
                            tracing::warn!(
                                target: "aether_mcp::rpc",
                                reason = %reason,
                                "hub closed the rpc connection; draining pending calls",
                            );
                            break;
                        }
                        // Hello / HelloAck / Call / Ping / Pong: a
                        // client-side router never expects these.
                        _ => continue,
                    };
                    // `cid == 0` is the wire-level out-of-band error
                    // sentinel (iamacoffeepot/aether#1271): the server
                    // couldn't decode the frame far enough to learn the
                    // real cid (e.g. an oversize body), so the
                    // structured error rides under id 0. Fan it out to
                    // every pending caller so the original failing
                    // call surfaces the wire error instead of hanging.
                    // A `ReplyEnd { cid: 0, Ok }` is not a valid wire
                    // shape — only `Err` frames carry the sentinel.
                    let fanout =
                        cid == 0 && matches!(&frame, WireFrame::ReplyEnd { result: Err(_), .. },);
                    let pending = router_pending
                        .lock()
                        .expect("router pending mutex is never poisoned");
                    if fanout {
                        for tx in pending.values() {
                            let _ = tx.send(frame.clone());
                        }
                    } else if let Some(tx) = pending.get(&cid) {
                        // A failed send just means a stray frame for an
                        // already-finished call — drop it.
                        let _ = tx.send(frame);
                    }
                    drop(pending);
                }
                // The router is exiting (peer `Bye` or `inbound` closed).
                // Mark the connection dead and drop every pending sender
                // — all under the `pending` lock, so a concurrent
                // `call_once` either registers before this (and sees
                // `None` when its sender drops here) or observes `dead`
                // after and bails without registering. Either way no
                // call hangs waiting on a router that will never run.
                let mut pending = router_pending
                    .lock()
                    .expect("router pending mutex is never poisoned");
                router_dead.store(true, Ordering::Release);
                pending.clear();
            })?;

        Ok(Self {
            client: Mutex::new(client),
            pending,
            dead,
            server,
            _reader: reader,
            _router: router,
        })
    }
}

/// One outbound RPC connection to the hub, shared across every MCP
/// tool call. Cheap to `Arc`-share into each per-session `Mcp`.
///
/// The live socket sits behind a swappable [`Connection`] cell, so a
/// hub restart is healed in place: `call()` re-dials `hub_addr` and
/// retries. The shared `Arc<RpcSession>` and the `call*(&self)`
/// signatures are unaffected.
pub struct RpcSession {
    /// The hub address dialed at startup, re-dialed on a dead socket.
    hub_addr: String,
    /// The live connection. Held behind a `std::sync::Mutex` only long
    /// enough to clone the `Arc` out — the socket work happens on the
    /// cloned handle, so steady-state calls never serialize here.
    conn: Mutex<Arc<Connection>>,
    /// Bumped on every successful re-dial. A `call()` snapshots it
    /// before acquiring [`Self::reconnect_lock`]; if it advanced while
    /// the caller waited, another task already healed the connection
    /// and this caller just retries against the fresh socket.
    generation: AtomicU64,
    /// Single-flight gate for re-dialing. When the hub drops, many
    /// in-flight calls fail at once; the gate ensures exactly one
    /// re-dials while the rest wait, then retry against the healed
    /// connection rather than each opening their own socket.
    reconnect_lock: AsyncMutex<()>,
}

impl RpcSession {
    /// Dial the hub's `RpcServerCapability` at `addr`, run the
    /// `Hello` / `HelloAck` handshake, and spawn the demux router
    /// thread. Blocking — call it off the async runtime.
    pub fn connect(addr: &str) -> anyhow::Result<Self> {
        let conn = Connection::establish(addr)?;
        Ok(Self {
            hub_addr: addr.to_owned(),
            conn: Mutex::new(Arc::new(conn)),
            generation: AtomicU64::new(0),
            reconnect_lock: AsyncMutex::new(()),
        })
    }

    /// The hub's `HelloAck` identity from the live connection.
    pub fn server(&self) -> PeerKind {
        self.live().server.clone()
    }

    /// Snapshot the live connection. Held only long enough to clone the
    /// `Arc`, so concurrent calls don't serialize on the socket.
    fn live(&self) -> Arc<Connection> {
        Arc::clone(
            &self
                .conn
                .lock()
                .expect("rpc connection mutex is never poisoned"),
        )
    }

    /// Re-dial the hub and swap in a fresh [`Connection`], single-flight
    /// across concurrent callers.
    ///
    /// `observed_generation` is the generation the caller saw before
    /// its call failed. After taking [`Self::reconnect_lock`] we re-read
    /// the generation: if it advanced, another task already re-dialed
    /// while we waited, so we return without opening a second socket and
    /// the caller retries against the connection that task healed.
    /// Otherwise we re-run the handshake (with bounded backoff) and
    /// atomically swap the cell — dropping the old `Connection` tears
    /// the dead socket down.
    async fn reconnect(&self, observed_generation: u64) -> anyhow::Result<()> {
        let _guard = self.reconnect_lock.lock().await;

        // Lost the race: someone already healed the connection. The
        // caller will retry against the fresh socket.
        if self.generation.load(Ordering::Acquire) != observed_generation {
            return Ok(());
        }

        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..RECONNECT_ATTEMPTS {
            if attempt > 0 {
                time::sleep(RECONNECT_BACKOFF).await;
            }
            let addr = self.hub_addr.clone();
            // The handshake is blocking; keep it off the runtime worker.
            let established = task::spawn_blocking(move || Connection::establish(&addr)).await;
            match established {
                Ok(Ok(conn)) => {
                    // Swap the cell. The old `Arc<Connection>` drops
                    // when the last in-flight call releases it; its
                    // `RpcReaderHandle::Drop` then tears the dead socket
                    // down and the old router thread exits.
                    *self
                        .conn
                        .lock()
                        .expect("rpc connection mutex is never poisoned") = Arc::new(conn);
                    self.generation.fetch_add(1, Ordering::Release);
                    tracing::info!(
                        target: "aether_mcp::rpc",
                        hub = %self.hub_addr,
                        "re-dialed the hub after a dead rpc session",
                    );
                    return Ok(());
                }
                Ok(Err(e)) => last_err = Some(e),
                Err(join_err) => {
                    last_err = Some(anyhow::anyhow!("reconnect task panicked: {join_err}"));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("rpc reconnect failed")))
    }

    /// Write a `Call` carrying `envelope` and await its terminal
    /// `ReplyEnd`, collecting every `ReplyEvent` seen in between.
    ///
    /// Self-healing: if the socket is dead — a write error, or the
    /// router closing this call's channel after a `Bye` — re-dial the
    /// hub and retry the call once. A still-down hub surfaces a clean
    /// error after the bounded re-dial attempts rather than hanging.
    pub async fn call(&self, envelope: MailEnvelope) -> anyhow::Result<Vec<MailEnvelope>> {
        match self.call_once(&envelope).await {
            Ok(events) => Ok(events),
            Err(CallError::Transport { generation, source }) => {
                tracing::warn!(
                    target: "aether_mcp::rpc",
                    error = %source,
                    "rpc call hit a dead socket; re-dialing the hub",
                );
                self.reconnect(generation).await?;
                // Retry once against the healed connection. A second
                // transport failure surfaces — we don't loop on a hub
                // that keeps dropping mid-call.
                self.call_once(&envelope)
                    .await
                    .map_err(CallError::into_anyhow)
            }
            Err(other) => Err(other.into_anyhow()),
        }
    }

    /// One attempt of [`Self::call`] against the live connection. A
    /// dead-socket terminal is returned as [`CallError::Transport`]
    /// (carrying the generation seen, so the re-dial is single-flight);
    /// a `ReplyEnd { Err }` is a clean call failure that must not
    /// trigger a re-dial.
    async fn call_once(&self, envelope: &MailEnvelope) -> Result<Vec<MailEnvelope>, CallError> {
        // Read the generation *before* snapshotting the connection: if a
        // re-dial races in between, `conn` is the fresher one and the
        // stale generation makes a later `reconnect` a no-op (the caller
        // retries against the already-healed socket) rather than a
        // redundant re-dial.
        let generation = self.generation.load(Ordering::Acquire);
        let conn = self.live();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let cid = register_call(&conn, envelope, tx, generation)?;

        let mut events = Vec::new();
        let outcome = loop {
            match rx.recv().await {
                Some(WireFrame::ReplyEvent { envelope, .. }) => events.push(envelope),
                Some(WireFrame::ReplyEnd { result, .. }) => {
                    break result
                        .map_err(|e| CallError::Call(anyhow::anyhow!("rpc call failed: {e:?}")));
                }
                // The router only ever routes ReplyEvent / ReplyEnd to
                // a pending entry.
                Some(_) => {}
                None => {
                    // The router dropped this call's sender — a `Bye`
                    // drained `pending` (dead socket). Re-dial + retry.
                    break Err(CallError::Transport {
                        generation,
                        source: anyhow::anyhow!("rpc connection closed before the call ended"),
                    });
                }
            }
        };
        conn.pending
            .lock()
            .expect("rpc pending mutex is never poisoned")
            .remove(&cid);
        outcome.map(|()| events)
    }

    /// [`Self::call`], expecting exactly one `ReplyEvent` — the shape
    /// of the engines-cap result kinds (`ListEnginesResult`, etc.).
    pub async fn call_one(&self, envelope: MailEnvelope) -> anyhow::Result<MailEnvelope> {
        let mut events = self.call(envelope).await?;
        match events.len() {
            1 => Ok(events.pop().expect("len checked")),
            n => Err(anyhow::anyhow!("expected exactly one reply event, got {n}")),
        }
    }

    /// Like [`Self::call`], but bounded by `timeout` and **partial-aware**:
    /// on timeout it returns the `ReplyEvent`s collected *so far* plus a
    /// `timed_out: true` flag, rather than discarding the in-flight
    /// future's buffered events (issue 1242).
    ///
    /// The collection loop runs *inside* the timeout scope — a plain
    /// `tokio::time::timeout(call(...))` would drop the `call` future on
    /// the timeout arm, losing both the partial `Vec` and the `cid`
    /// de-registration. Here the `select!` between `rx.recv()` and a
    /// `sleep(timeout)` writes into a buffer this fn owns, and the `cid`
    /// is removed from `pending` on every exit (settle, timeout, or
    /// transport error), so a timed-out call leaks no `pending` entry.
    ///
    /// Self-healing on a dead socket is the same single re-dial + retry
    /// as [`Self::call`]; the `timed_out` flag is `false` on every
    /// non-timeout terminal (settle / error).
    pub async fn call_collecting(
        &self,
        envelope: MailEnvelope,
        timeout: Duration,
    ) -> anyhow::Result<(Vec<MailEnvelope>, bool)> {
        match self.call_collecting_once(&envelope, timeout).await {
            Ok(outcome) => Ok(outcome),
            Err(CallError::Transport { generation, source }) => {
                tracing::warn!(
                    target: "aether_mcp::rpc",
                    error = %source,
                    "rpc call_collecting hit a dead socket; re-dialing the hub",
                );
                self.reconnect(generation).await?;
                self.call_collecting_once(&envelope, timeout)
                    .await
                    .map_err(CallError::into_anyhow)
            }
            Err(other) => Err(other.into_anyhow()),
        }
    }

    /// One attempt of [`Self::call_collecting`] against the live
    /// connection. Mirrors [`Self::call_once`]'s register / write /
    /// drain / deregister bracket, adding the timeout arm.
    async fn call_collecting_once(
        &self,
        envelope: &MailEnvelope,
        timeout: Duration,
    ) -> Result<(Vec<MailEnvelope>, bool), CallError> {
        let generation = self.generation.load(Ordering::Acquire);
        let conn = self.live();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let cid = register_call(&conn, envelope, tx, generation)?;

        let mut events = Vec::new();
        // `sleep` is created once, outside the loop, so its deadline is
        // the wall-clock cap on the *whole* collection — not reset on
        // each `ReplyEvent`.
        let sleep = time::sleep(timeout);
        tokio::pin!(sleep);
        let outcome = loop {
            tokio::select! {
                frame = rx.recv() => match frame {
                    Some(WireFrame::ReplyEvent { envelope, .. }) => events.push(envelope),
                    Some(WireFrame::ReplyEnd { result, .. }) => {
                        break result
                            .map(|()| false)
                            .map_err(|e| {
                                CallError::Call(anyhow::anyhow!("rpc call failed: {e:?}"))
                            });
                    }
                    Some(_) => {}
                    None => {
                        break Err(CallError::Transport {
                            generation,
                            source: anyhow::anyhow!(
                                "rpc connection closed before the call ended"
                            ),
                        });
                    }
                },
                () = &mut sleep => break Ok(true),
            }
        };
        conn.pending
            .lock()
            .expect("rpc pending mutex is never poisoned")
            .remove(&cid);
        outcome.map(|timed_out| (events, timed_out))
    }

    /// Write a `Call` carrying `envelope` and return *without* awaiting
    /// any reply — the fire-and-forget path (issue 1242). The server
    /// still answers with `ReplyEnd` server-side, but no `pending` entry
    /// is registered for the `cid`, so the router drops the inbound
    /// `ReplyEvent` / `ReplyEnd` as unrouted (its `get(&cid)` misses) and
    /// no state accumulates. A dead socket re-dials once, mirroring
    /// [`Self::call`], so a poke against a just-bounced hub still lands.
    pub async fn fire(&self, envelope: MailEnvelope) -> anyhow::Result<()> {
        match self.fire_once(&envelope) {
            Ok(()) => Ok(()),
            Err(CallError::Transport { generation, source }) => {
                tracing::warn!(
                    target: "aether_mcp::rpc",
                    error = %source,
                    "rpc fire hit a dead socket; re-dialing the hub",
                );
                self.reconnect(generation).await?;
                self.fire_once(&envelope).map_err(CallError::into_anyhow)
            }
            Err(other) => Err(other.into_anyhow()),
        }
    }

    /// One attempt of [`Self::fire`]: write the `Call` on the live
    /// connection without registering a `pending` entry. Synchronous —
    /// there's nothing to await.
    fn fire_once(&self, envelope: &MailEnvelope) -> Result<(), CallError> {
        let generation = self.generation.load(Ordering::Acquire);
        let conn = self.live();
        if conn.dead.load(Ordering::Acquire) {
            return Err(CallError::Transport {
                generation,
                source: anyhow::anyhow!("rpc connection is closed"),
            });
        }
        conn.client
            .lock()
            .expect("rpc client mutex is never poisoned")
            .call(envelope.clone())
            .map(|_cid| ())
            .map_err(|e| classify_write_error(&e, generation))
    }
}

/// Outcome of one [`RpcSession::call_once`]. The `Transport` variant is
/// the only one that triggers a re-dial; `Call` is a clean failure the
/// hub returned over a healthy socket and is surfaced as-is.
enum CallError {
    /// The socket is dead (write error, or the call's channel closed
    /// after a `Bye`). Carries the connection generation the caller
    /// saw, so [`RpcSession::reconnect`] stays single-flight.
    Transport {
        generation: u64,
        source: anyhow::Error,
    },
    /// The hub answered with `ReplyEnd { Err }` — a clean call failure,
    /// not a transport problem.
    Call(anyhow::Error),
}

impl CallError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Transport { source, .. } | Self::Call(source) => source,
        }
    }
}

/// Classify an [`RpcClientError`] from the outbound write path
/// (iamacoffeepot/aether#1271). `EncodeTooLarge` is a clean
/// client-side rejection — the bytes never hit the wire, so the
/// socket is still healthy and a re-dial would only re-encode + fail
/// the same way. Everything else (TCP write errors, broken pipes) is
/// a transport failure that warrants a single re-dial + retry.
fn classify_write_error(e: &RpcClientError, generation: u64) -> CallError {
    if matches!(e, RpcClientError::Frame(FrameError::EncodeTooLarge { .. })) {
        return CallError::Call(anyhow::anyhow!("rpc call encode rejected: {e}"));
    }
    CallError::Transport {
        generation,
        source: anyhow::anyhow!("rpc call write failed: {e}"),
    }
}

/// Send a `Call` over `conn`, register the resulting `cid` against
/// `tx` in `conn.pending`, and return the cid. Bundles the dead-socket
/// check, the write, and the pending registration — every call shape
/// (`call_once`, `call_collecting_once`) does the same dance, so
/// factoring it here keeps Qodana's `DuplicatedCode` quiet and
/// localizes the lock-section invariant. All steps run under one hold
/// of `conn.pending` so the check-and-register is atomic against the
/// router's death.
fn register_call(
    conn: &Connection,
    envelope: &MailEnvelope,
    tx: mpsc::UnboundedSender<WireFrame>,
    generation: u64,
) -> Result<u64, CallError> {
    let mut pending = conn
        .pending
        .lock()
        .expect("rpc pending mutex is never poisoned");
    if conn.dead.load(Ordering::Acquire) {
        return Err(CallError::Transport {
            generation,
            source: anyhow::anyhow!("rpc connection is closed"),
        });
    }
    let cid = conn
        .client
        .lock()
        .expect("rpc client mutex is never poisoned")
        .call(envelope.clone())
        .map_err(|e| classify_write_error(&e, generation))?;
    pending.insert(cid, tx);
    drop(pending);
    Ok(cid)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
mod tests {
    use super::RpcSession;
    use aether_capabilities::rpc::{
        HelloAck, MailEnvelope, MailboxAddress, PeerKind, WIRE_VERSION, WireFrame,
    };
    use aether_codec::frame::{read_frame, write_frame};
    use aether_data::{KindId, MailboxId};
    use std::io::{BufReader, ErrorKind};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};
    use tokio::task;

    /// A controllable stand-in for the hub: it accepts one connection,
    /// runs the `Hello` / `HelloAck` handshake, then answers each
    /// `Call { cid, envelope }` with a single `ReplyEvent` echoing the
    /// envelope followed by `ReplyEnd { Ok }`. The whole point is that
    /// the test owns its lifetime: [`FakeHub::stop`] tears the listener
    /// and connection down (the `RpcSession` reader observes EOF and
    /// surfaces the synthetic `Bye`), and a fresh [`FakeHub::serve`] on
    /// the *same* port simulates the hub coming back.
    struct FakeHub {
        shutdown: Arc<AtomicBool>,
        /// Clones of every accepted connection's stream, so `stop` can
        /// `shutdown()` them and unblock the session threads' reads.
        conns: Arc<Mutex<Vec<TcpStream>>>,
        port: u16,
        thread: Option<JoinHandle<()>>,
    }

    /// How a [`FakeHub`] session answers each `Call` (issue 1242 tests).
    /// The default echo (`reply_events: 1`, `send_end: true`) is the
    /// pre-1242 behaviour the older re-dial tests depend on.
    #[derive(Clone, Copy)]
    struct ServeMode {
        /// How many `ReplyEvent`s (each echoing the call's envelope) to
        /// emit before the terminal. `>1` exercises multi-reply
        /// collection.
        reply_events: usize,
        /// Whether to write the terminal `ReplyEnd { Ok }`. `false`
        /// withholds it so a `call_collecting` hits its timeout with the
        /// chain still "open" — the partial-on-timeout path.
        send_end: bool,
    }

    impl ServeMode {
        /// One `ReplyEvent` + `ReplyEnd` — the original echo behaviour.
        fn echo() -> Self {
            Self {
                reply_events: 1,
                send_end: true,
            }
        }
    }

    impl FakeHub {
        /// Bind, accept, handshake, and echo `Call`s on `port`. `port`
        /// 0 lets the OS pick; the chosen port is returned alongside.
        fn serve(port: u16) -> (Self, u16) {
            Self::serve_with(port, ServeMode::echo())
        }

        /// [`Self::serve`] with an explicit [`ServeMode`] — lets a test
        /// drive multi-reply collection or withhold the terminal
        /// `ReplyEnd` to force a timeout.
        fn serve_with(port: u16, mode: ServeMode) -> (Self, u16) {
            let listener = TcpListener::bind(("127.0.0.1", port)).expect("fake hub bind");
            let bound_port = listener.local_addr().expect("local_addr").port();
            // Non-blocking accept so the loop can observe the shutdown
            // flag promptly; each session reads on its own thread with a
            // blocking socket (no read timeout → no partial-frame race).
            listener
                .set_nonblocking(true)
                .expect("fake hub set_nonblocking");

            let shutdown = Arc::new(AtomicBool::new(false));
            let conns: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
            let shutdown_for_thread = Arc::clone(&shutdown);
            let conns_for_thread = Arc::clone(&conns);
            let thread = thread::Builder::new()
                .name("fake-hub".into())
                .spawn(move || {
                    let mut sessions: Vec<JoinHandle<()>> = Vec::new();
                    while !shutdown_for_thread.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                stream
                                    .set_nonblocking(false)
                                    .expect("session socket blocking");
                                conns_for_thread
                                    .lock()
                                    .expect("conns mutex")
                                    .push(stream.try_clone().expect("clone conn"));
                                let shutdown = Arc::clone(&shutdown_for_thread);
                                sessions.push(
                                    thread::Builder::new()
                                        .name("fake-hub-session".into())
                                        .spawn(move || Self::run_session(&stream, &shutdown, mode))
                                        .expect("spawn session"),
                                );
                            }
                            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                    for s in sessions {
                        let _ = s.join();
                    }
                })
                .expect("spawn fake hub");

            (
                Self {
                    shutdown,
                    conns,
                    port: bound_port,
                    thread: Some(thread),
                },
                bound_port,
            )
        }

        /// Handshake + blocking echo loop for one accepted connection.
        /// Per [`ServeMode`] it emits `mode.reply_events` `ReplyEvent`s
        /// (each echoing the call's envelope) and, when `mode.send_end`,
        /// the terminal `ReplyEnd { Ok }`. Returns when the connection
        /// closes (`stop` shuts it down, or the client drops) —
        /// `read_frame` then errors.
        fn run_session(stream: &TcpStream, shutdown: &AtomicBool, mode: ServeMode) {
            let mut write_half = stream.try_clone().expect("clone write half");
            let mut reader = BufReader::new(stream.try_clone().expect("clone read half"));

            let hello: WireFrame = match read_frame(&mut reader) {
                Ok(f) => f,
                Err(_) => return,
            };
            if !matches!(hello, WireFrame::Hello(_)) {
                return;
            }
            write_frame(
                &mut write_half,
                &WireFrame::HelloAck(HelloAck {
                    wire_version: WIRE_VERSION,
                    server: PeerKind::Substrate {
                        engine_name: "fake-hub".into(),
                        engine_version: "0.0.0".into(),
                        kinds: vec![],
                    },
                }),
            )
            .expect("write HelloAck");

            while !shutdown.load(Ordering::Acquire) {
                let frame: WireFrame = match read_frame(&mut reader) {
                    Ok(f) => f,
                    // Client closed or `stop` shut the socket down.
                    Err(_) => return,
                };
                if let WireFrame::Call {
                    cid: Some(cid),
                    envelope,
                } = frame
                {
                    for _ in 0..mode.reply_events {
                        write_frame(
                            &mut write_half,
                            &WireFrame::ReplyEvent {
                                cid,
                                envelope: envelope.clone(),
                            },
                        )
                        .expect("write ReplyEvent");
                    }
                    if mode.send_end {
                        write_frame(
                            &mut write_half,
                            &WireFrame::ReplyEnd {
                                cid,
                                result: Ok(()),
                            },
                        )
                        .expect("write ReplyEnd");
                    }
                }
            }
        }

        /// Tear the hub down: flag shutdown, shut every accepted
        /// connection (waking the blocked session reads), and join.
        /// After this returns the port is free for a fresh
        /// [`FakeHub::serve`] (std sets `SO_REUSEADDR`).
        fn stop(mut self) {
            self.shutdown.store(true, Ordering::Release);
            for conn in self.conns.lock().expect("conns mutex").iter() {
                let _ = conn.shutdown(Shutdown::Both);
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            // A TIME_WAIT window can briefly hold the port; spin until a
            // fresh bind succeeds so the caller's immediate re-`serve`
            // on the same port can't flake.
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                match TcpListener::bind(("127.0.0.1", self.port)) {
                    Ok(_) => return,
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
        }
    }

    /// A throwaway envelope to a local mailbox — the fake hub echoes it
    /// back verbatim, so the contents don't matter.
    fn probe_envelope() -> MailEnvelope {
        MailEnvelope {
            to: MailboxAddress::local(MailboxId(1)),
            from: None,
            kind: KindId(1),
            correlation_id: None,
            payload: vec![1, 2, 3],
        }
    }

    /// Contention/backoff-sensitive tests live in `mod heavy`: the restart
    /// path races a socket close + same-port re-bind, so it is timing
    /// sensitive — serialized into the `serial-heavy` nextest group
    /// (`.config/nextest.toml`) and selected by `scripts/flake-soak.sh` for
    /// fresh-process soak repetition.
    mod heavy {
        /// The load-bearing fix (iamacoffeepot/aether#1212): a hub restart
        /// under a live `RpcSession` is healed in place. A first call
        /// round-trips; the hub then dies (the reader sidecar surfaces the
        /// synthetic `Bye`, draining `pending` so the in-flight call fails
        /// on the dead socket); the hub comes back on the same port; and
        /// the next call re-dials and succeeds rather than erroring forever.
        #[tokio::test]
        async fn call_redials_after_hub_restart() {
            super::run_redial_after_hub_restart().await;
        }
    }

    async fn run_redial_after_hub_restart() {
        // Boot the first hub on an OS-picked port.
        let (hub, port) = FakeHub::serve(0);
        let session =
            task::spawn_blocking(move || RpcSession::connect(&format!("127.0.0.1:{port}")))
                .await
                .expect("connect task")
                .expect("first connect");
        let session = Arc::new(session);

        // First call against the live hub round-trips.
        session
            .call(probe_envelope())
            .await
            .expect("first call succeeds against the live hub");

        // Kill the hub. The client reader sees EOF and the router
        // drains every pending call.
        hub.stop();

        // A call issued while the hub is down attempts a bounded
        // re-dial and fails cleanly (not a hang). This proves the
        // dead-socket path surfaces an error rather than blocking.
        let while_down = session.call(probe_envelope()).await;
        assert!(
            while_down.is_err(),
            "a call against a still-down hub must error cleanly, not hang",
        );

        // The hub comes back on the same port.
        let (hub2, port2) = FakeHub::serve(port);
        assert_eq!(port2, port, "fake hub re-bound the same port");

        // The next call must re-dial and succeed — not error out
        // permanently on the dead socket.
        session
            .call(probe_envelope())
            .await
            .expect("call must re-dial + succeed once the hub is back");

        hub2.stop();
    }

    /// Many concurrent calls that all fail on one hub death trigger a
    /// *single* re-dial, not a thundering herd of sockets. After the
    /// hub returns, every in-flight call recovers.
    #[tokio::test]
    async fn concurrent_calls_share_a_single_redial() {
        let (hub, port) = FakeHub::serve(0);
        let session =
            task::spawn_blocking(move || RpcSession::connect(&format!("127.0.0.1:{port}")))
                .await
                .expect("connect task")
                .expect("first connect");
        let session = Arc::new(session);

        // Warm the connection.
        session.call(probe_envelope()).await.expect("warm-up call");

        // Drop the hub, then immediately bring it back on the same
        // port so the racing re-dials have a live target.
        hub.stop();
        let (hub2, _) = FakeHub::serve(port);

        // Fire a burst of concurrent calls. They contend on the
        // single-flight reconnect gate; exactly one re-dials and the
        // rest retry against the connection it heals.
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let session = Arc::clone(&session);
            tasks.push(tokio::spawn(
                async move { session.call(probe_envelope()).await },
            ));
        }
        for t in tasks {
            t.await
                .expect("join")
                .expect("every concurrent call recovers after the single re-dial");
        }

        // One successful re-dial advanced the generation exactly once.
        assert_eq!(
            session.generation.load(Ordering::Acquire),
            1,
            "concurrent failures must share one re-dial, not stampede",
        );

        hub2.stop();
    }

    /// The count of registered `pending` entries on the live connection
    /// — a timed-out / fired call must leave this at zero, proving no
    /// `cid` leaks (issue 1242).
    fn pending_count(session: &RpcSession) -> usize {
        session.live().pending.lock().expect("pending mutex").len()
    }

    /// Issue 1242: `call_collecting` surfaces every `ReplyEvent` the hub
    /// emitted before the terminal, in arrival order, with
    /// `timed_out == false` — and de-registers the `cid` so `pending` is
    /// clean afterward. Two reply events per call exercises the
    /// multi-reply collection the single-event `call_one` discarded.
    #[tokio::test]
    async fn call_collecting_returns_events_in_order() {
        let (hub, port) = FakeHub::serve_with(
            0,
            ServeMode {
                reply_events: 2,
                send_end: true,
            },
        );
        let session =
            task::spawn_blocking(move || RpcSession::connect(&format!("127.0.0.1:{port}")))
                .await
                .expect("connect task")
                .expect("connect");

        let (events, timed_out) = session
            .call_collecting(probe_envelope(), Duration::from_secs(5))
            .await
            .expect("call_collecting succeeds against the live hub");

        assert!(!timed_out, "a settled call is not a timeout");
        assert_eq!(events.len(), 2, "both echoed reply events are surfaced");
        for event in &events {
            assert_eq!(
                event.payload,
                probe_envelope().payload,
                "each reply echoes the probe envelope",
            );
        }
        assert_eq!(
            pending_count(&session),
            0,
            "a settled call de-registers its cid",
        );

        hub.stop();
    }

    /// Issue 1242: when the hub withholds the terminal `ReplyEnd`,
    /// `call_collecting` returns the events collected before the short
    /// timeout with `timed_out == true`, and leaves no `cid` in
    /// `pending` — the partial-on-timeout path the new logic must get
    /// right (the buffer is owned outside the `select!`, so it survives
    /// the timeout arm, and the `cid` is removed on every exit).
    #[tokio::test]
    async fn call_collecting_times_out_with_partial_and_clean_pending() {
        let (hub, port) = FakeHub::serve_with(
            0,
            ServeMode {
                reply_events: 1,
                // No terminal — the call can only end via the timeout.
                send_end: false,
            },
        );
        let session =
            task::spawn_blocking(move || RpcSession::connect(&format!("127.0.0.1:{port}")))
                .await
                .expect("connect task")
                .expect("connect");

        let (events, timed_out) = session
            .call_collecting(probe_envelope(), Duration::from_millis(250))
            .await
            .expect("a timeout is not a transport error");

        assert!(timed_out, "withholding ReplyEnd forces the timeout arm");
        assert_eq!(
            events.len(),
            1,
            "the reply that arrived before the timeout is still returned",
        );
        assert_eq!(
            pending_count(&session),
            0,
            "a timed-out call must not leak its cid in pending",
        );

        hub.stop();
    }

    /// Issue 1242: `fire` writes the `Call` and returns without awaiting
    /// any reply, registering no `pending` entry — the fire-and-forget
    /// path. The hub's eventual `ReplyEnd` is dropped as an unrouted
    /// frame (its `cid` has no `pending` sender), so no state
    /// accumulates.
    #[tokio::test]
    async fn fire_does_not_register_pending() {
        let (hub, port) = FakeHub::serve(0);
        let session =
            task::spawn_blocking(move || RpcSession::connect(&format!("127.0.0.1:{port}")))
                .await
                .expect("connect task")
                .expect("connect");

        session
            .fire(probe_envelope())
            .await
            .expect("fire writes the call");

        assert_eq!(
            pending_count(&session),
            0,
            "fire registers nothing in pending",
        );

        hub.stop();
    }
}
