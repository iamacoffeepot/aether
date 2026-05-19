//! `RpcSession` — the `aether-mcp` side of one outbound RPC connection
//! to the hub (issue 763 P5b).
//!
//! Wraps [`RpcClient`] / [`RpcConnection`] (issue 763 P1) with the
//! demultiplexing the MCP tool surface needs: many tool calls share
//! the one socket, so a router thread drains the connection's single
//! inbound channel and fans each `ReplyEvent` / `ReplyEnd` to the
//! [`RpcSession::call`] future waiting on that `cid`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aether_capabilities::rpc::{
    MailEnvelope, PeerKind, RpcClient, RpcConnection, RpcReaderHandle, WireFrame,
};
use std::thread;
use std::thread::JoinHandle;
use tokio::sync::mpsc;

/// One outbound RPC connection to the hub, shared across every MCP
/// tool call. Cheap to `Arc`-share into each per-session `Mcp`.
pub struct RpcSession {
    /// Write half. Behind a `std::sync::Mutex` so concurrent tool
    /// calls serialize their `Call` writes — and so a write + the
    /// matching `pending` registration happen under one lock section,
    /// closing the race against the router thread.
    client: Mutex<RpcClient>,
    /// `cid` → the channel feeding the `call()` future awaiting it.
    /// The router thread routes inbound frames here; `call()` registers
    /// an entry before its write is visible and clears it on `ReplyEnd`.
    pending: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<WireFrame>>>>,
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

impl RpcSession {
    /// Dial the hub's `RpcServerCapability` at `addr`, run the
    /// `Hello` / `HelloAck` handshake, and spawn the demux router
    /// thread. Blocking — call it off the async runtime.
    pub fn connect(addr: &str) -> anyhow::Result<Self> {
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

        let pending: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<WireFrame>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let router_pending = Arc::clone(&pending);
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
                            // Drop every pending sender so each waiting
                            // `call()` future sees `None` and errors
                            // out rather than hanging forever.
                            router_pending
                                .lock()
                                .expect("router pending mutex is never poisoned")
                                .clear();
                            break;
                        }
                        // Hello / HelloAck / Call / Ping / Pong: a
                        // client-side router never expects these.
                        _ => continue,
                    };
                    if let Some(tx) = router_pending
                        .lock()
                        .expect("router pending mutex is never poisoned")
                        .get(&cid)
                    {
                        // A failed send just means a stray frame for an
                        // already-finished call — drop it.
                        let _ = tx.send(frame);
                    }
                }
            })?;

        Ok(Self {
            client: Mutex::new(client),
            pending,
            server,
            _reader: reader,
            _router: router,
        })
    }

    /// The hub's `HelloAck` identity.
    pub fn server(&self) -> &PeerKind {
        &self.server
    }

    /// Write a `Call` carrying `envelope` and await its terminal
    /// `ReplyEnd`, collecting every `ReplyEvent` seen in between.
    ///
    /// The `pending` registration shares a lock section with the
    /// socket write, so the router thread can't route this call's
    /// reply before the entry exists.
    pub async fn call(&self, envelope: MailEnvelope) -> anyhow::Result<Vec<MailEnvelope>> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cid = {
            let mut pending = self
                .pending
                .lock()
                .expect("rpc pending mutex is never poisoned");
            let cid = self
                .client
                .lock()
                .expect("rpc client mutex is never poisoned")
                .call(envelope)
                .map_err(|e| anyhow::anyhow!("rpc call write failed: {e}"))?;
            pending.insert(cid, tx);
            cid
        };

        let mut events = Vec::new();
        let outcome = loop {
            match rx.recv().await {
                Some(WireFrame::ReplyEvent { envelope, .. }) => events.push(envelope),
                Some(WireFrame::ReplyEnd { result, .. }) => {
                    break result.map_err(|e| anyhow::anyhow!("rpc call failed: {e:?}"));
                }
                // The router only ever routes ReplyEvent / ReplyEnd to
                // a pending entry.
                Some(_) => {}
                None => {
                    break Err(anyhow::anyhow!(
                        "rpc connection closed before the call ended"
                    ));
                }
            }
        };
        self.pending
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

    /// [`Self::call`], discarding any reply events — for mail whose
    /// only interesting outcome is "did the call reach the substrate
    /// and settle". The terminal `ReplyEnd` still gates the result.
    pub async fn call_settled(&self, envelope: MailEnvelope) -> anyhow::Result<()> {
        self.call(envelope).await.map(|_events| ())
    }
}
