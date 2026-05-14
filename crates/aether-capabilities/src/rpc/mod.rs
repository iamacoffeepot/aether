//! `aether.rpc` — generic TCP RPC transport (issues 750, 763).
//!
//! - [`wire`] — the type-erased wire vocabulary: length-prefix postcard
//!   [`WireFrame`]s carrying mail envelopes. Endpoints are mail kinds,
//!   not request enums, so any kind two peers share is reachable
//!   without a wire change.
//! - [`server`] — [`RpcServerCapability`], the singleton actor that
//!   binds a TCP listener, accepts connections, and dispatches inbound
//!   `Call` envelopes into the local actor system (issue 750).
//! - [`client`] — [`RpcClient`], the outbound counterpart: dials an RPC
//!   server, runs the handshake, and frames inbound `WireFrame`s onto
//!   an mpsc (issue 763 P1). Native-only.
//!
//! See issues 750 and 763 for the full design.

pub mod server;
pub mod wire;

#[cfg(not(target_arch = "wasm32"))]
pub mod client;

// Shared round-trip test scaffolding (echo actor + its kinds), used by
// both the `server` and `client` test modules.
#[cfg(test)]
mod test_echo;

#[cfg(not(target_arch = "wasm32"))]
pub use client::{RpcClient, RpcClientError, RpcConnection, RpcReaderHandle};
pub use server::{RpcServerCapability, rpc_server_mailbox_id};
#[cfg(not(target_arch = "wasm32"))]
pub use server::{RpcServerConfig, RpcServerHandle};
pub use wire::{
    Hello, HelloAck, KindDescriptor, MailEnvelope, MailboxAddress, PeerKind, RpcError,
    WIRE_VERSION, WireFrame,
};
