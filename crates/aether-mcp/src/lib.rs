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
//! The `McpServerCapability` identity, the tool/resource registries, the
//! response-resource store, the `aether-mcp-derive` authoring macros
//! (`#[mcp::tool]` / `#[mcp::router]` / `#[mcp::reply]`), and the strict
//! stateless [`client`](https://modelcontextprotocol.io) all belong to later
//! steps. The module surface below is shaped for those consumers: the
//! protocol layer parses and renders, and never dispatches.

// The library half is additive to the `rmcp` binaries in this package; the
// binary modules (`args`, `reverse`, `rpc`, `tools`) stay bin-private and are
// deliberately not declared here.

pub mod configuration;
pub mod kinds;
pub mod schema;

// The message model is the shared vocabulary of the server actor and the
// strict stateless client. A marker-only provider declaring tools needs
// neither, so it compiles under either feature rather than always.
#[cfg(any(feature = "runtime", feature = "client"))]
pub mod protocol;

pub use configuration::McpServerConfiguration;
pub use kinds::*;
