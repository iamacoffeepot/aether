//! `aether-rpc` — the RPC wire vocabulary + the `Call` client primitive,
//! plus the decentralized trace-tree walk (ADR-0102, extracted from
//! `aether-capabilities`).
//!
//! This crate is deliberately free of any path to `aether-substrate`,
//! wasmtime, or wgpu: it is the pure-data wire layer the out-of-process
//! MCP coordinator (`aether-mcp`) speaks, and the crate boundary is what
//! holds that "no native deps" invariant under cargo's workspace-wide
//! feature unification — a feature flag on the host crate cannot.
//!
//! - [`rpc`] — the type-erased wire vocabulary ([`WireFrame`] and its
//!   substructs) plus [`RpcClient`], the native-only outbound client.
//! - [`trace_walk`] — [`TreeWalk`], the transport-agnostic guided walk
//!   that reconstructs one root's mail tree across the per-actor trace
//!   rings (ADR-0086 Phase 3b).
//!
//! The substrate-bound `RpcServerCapability` stays in
//! `aether-capabilities`, which re-exports `aether_rpc::rpc::*` and
//! `aether_rpc::trace_walk::*` at their original paths so existing call
//! sites compile unchanged.
//!
//! [`WireFrame`]: rpc::WireFrame
//! [`RpcClient`]: rpc::RpcClient
//! [`TreeWalk`]: trace_walk::TreeWalk

pub mod rpc;
pub mod trace_walk;
