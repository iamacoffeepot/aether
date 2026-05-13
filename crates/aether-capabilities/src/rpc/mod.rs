//! `aether.rpc` — generic TCP RPC capability (issue 750).
//!
//! Phase 1 (this module): wire vocabulary and the untyped envelope
//! dispatch surface on `NativeCtx::send_envelope_traced`. The
//! `RpcServerCapability` actor itself lands in phase 2, alongside
//! the accept-thread + per-connection reader machinery. Phase 3
//! wires it into the hub / substrate chassis on a configurable port.
//!
//! See issue 750 for the full design.

pub mod wire;

pub use wire::{
    Hello, HelloAck, KindDescriptor, MailEnvelope, MailboxAddress, PeerKind, RpcError,
    WIRE_VERSION, WireFrame,
};
