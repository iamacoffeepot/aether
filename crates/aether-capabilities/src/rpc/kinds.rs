//! `aether.rpc` mail kinds owned by the RPC server capability (ADR-0121).

use serde::{Deserialize, Serialize};

/// `aether.rpc.inbound_ready` — sidecar accept / read thread →
/// `RpcServerCapability` dispatcher wake. Issue 750. Mirrors the
/// `ConnectionReady` / `SessionDataReady` pattern for `aether.tcp`:
/// the sidecar pushes work over an internal mpsc and fires this
/// (empty-payload) mail at the cap's mailbox so the dispatcher
/// handler drains the queue. The mpsc carries the live data
/// (`TcpStream`, frame bytes, close reason) — a `TcpStream` isn't
/// wire-shaped and a frame's payload may be megabytes, so the mail
/// is only the wakeup signal.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.rpc.inbound_ready")]
pub struct RpcInboundReady {}
