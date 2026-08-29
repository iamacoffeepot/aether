//! Aether's owned Model Context Protocol library, revision **2025-06-18**.
//!
//! The workspace owns the protocol message model, method surface, schema
//! translation, and error mapping. No `rmcp`, no other protocol
//! implementation crate, no `schemars`, and no vendored fork participates.
//! The `rmcp` executable in this same package is the outgoing out-of-process
//! server; it is untouched by this library and disappears at the end of the
//! design's ordered migration.
//!
//! ## Crate shape
//!
//! The package name stays `aether-mcp` — `mcp` is the protocol's established
//! proper name across the checkout (`.mcp.json`, `.codex/config.toml`, the
//! `mcp__aether-hub__*` tool prefix), and a longer package name would create
//! a second name for one boundary. The capability's actor namespace is
//! [`SERVER_NAMESPACE`].
//!
//! Per ADR-0122 the layers are:
//!
//! - **Always available** — [`kinds`] (the capability's wire vocabulary),
//!   [`configuration`] (the resolved config domain struct), and [`schema`]
//!   (the `SchemaType` → JSON Schema translator and the client-value
//!   validator). None of these name `aether_substrate`, so a provider that
//!   only declares tools takes this crate `default-features = false`.
//! - **`runtime` or `client`** — [`protocol`], the message model both halves
//!   speak.
//! - **`runtime`** — the native server actor and its registries. Today the
//!   feature carries only the `Config` derive's codegen on
//!   [`McpServerConfiguration`]; the actor arrives in the runtime slice.
//!
//! ## What this slice does not contain
//!
//! The `aether-mcp-derive` authoring macros (`#[mcp::tool]` /
//! `#[mcp::router]` / `#[mcp::reply]`) and the strict stateless client belong
//! to later steps. The module surface below is shaped for those consumers: the
//! protocol layer parses and renders, and never dispatches; the runtime layer
//! decides, and never parses HTTP off a socket.

// ADR-0131's self-alias pattern: the `#[http::router]` macro emits absolute
// `::aether_mcp::…` paths for the support types it names, and this crate uses
// its own tool surface in the runtime half.
extern crate self as aether_mcp;

// The library half is additive to the `rmcp` binaries in this package; the
// binary modules (`args`, `reverse`, `rpc`, `tools`) stay bin-private and are
// deliberately not declared here.

pub mod configuration;
pub mod kinds;
pub mod schema;
pub mod tool;

// The message model is the shared vocabulary of the server actor and the
// strict stateless client. A marker-only provider declaring tools needs
// neither, so it compiles under either feature rather than always.
#[cfg(any(feature = "runtime", feature = "client"))]
pub mod protocol;

pub use configuration::McpServerConfiguration;
pub use kinds::*;
pub use tool::{Context, Outcome};

// The tool-authoring macros, re-exported beside the runtime types they compile
// down to — a provider writes `#[mcp::router]` / `#[mcp::tool]` /
// `#[mcp::reply]` next to `mcp::Context` / `mcp::Outcome` / `mcp::ToolError`.
// `tool` names both a module and an attribute macro here; they occupy
// different namespaces, so `mcp::tool::Context` and `#[mcp::tool]` both
// resolve.
pub use aether_mcp_derive::{reply, router, tool};

// The kind types the struct-hosted `#[actor]` below lifts out of the runtime
// module must resolve at *this* file's root: the harvest reads `runtime/mod.rs`
// off disk and emits one `HandlesKind<K>` marker per handler against the
// identity, naming each kind unqualified. The `#[http::route]` method in that
// impl is deliberately absent from this list — the HTTP macro expands after the
// harvest and its minted route kind is stamped dynamically at dispatch
// (ADR-0131), so the identity lifts no marker for it.
// The capability's own kinds arrive through the `pub use kinds::*` above; only
// the two it borrows from `aether-kinds` need naming here.
use aether_kinds::MonitorNotice;
use aether_kinds::trace::Settled;

/// `aether.mcp.server` capability **identity** (ADR-0122 identity/runtime
/// split, ADR-0123 struct-hosted form).
///
/// A zero-sized type carrying only the addressing: `Addressable`, one
/// `HandlesKind` marker per handler, and the name-inventory entry, all emitted
/// against this struct by `#[actor]` from the sibling `runtime` module read off
/// disk. The state-bearing half — the tool and resource registries, the
/// admission counters, the pending-call table, the ephemeral response store —
/// lives behind the one `feature = "runtime"` gate below, so a provider that
/// only declares tools can address `McpServerCapability` without pulling the
/// substrate through.
///
/// It owns no socket. The endpoint is one `#[http::route(any, "/mcp")]`
/// registered with `HttpServerCapability`, so every listener concern — bind
/// address, body ceiling, connection limits — belongs to that capability's
/// configuration rather than this one's.
#[aether_actor::actor(singleton, root)]
pub struct McpServerCapability;

// The runtime half — the whole `aether_substrate`-typed surface — lives in the
// `runtime/` submodule, gated once here.
#[cfg(feature = "runtime")]
mod runtime;

#[cfg(feature = "runtime")]
pub use runtime::McpServerState;
