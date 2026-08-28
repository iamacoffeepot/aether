//! The hub's Model Context Protocol tool provider.
//!
//! The design's step 4 mounts a *shadow* endpoint on the hub: the chassis
//! gains `HttpServerCapability` and `McpServerCapability`, and this module
//! supplies the hub-specific provider actor that answers tool calls. The
//! generic protocol crate carries no hub policy — it parses, admits, and
//! dispatches; what a hub tool *means* lives here, beside the chassis that
//! composes it.
//!
//! The layout mirrors Bloomery's routed-capability shape one level down:
//! [`provider`] is the actor — the single `#[mcp::router]` implementation
//! carrying every `#[mcp::tool]` method and its reply mapping — and a sibling
//! module per tool group owns that group's boundary types and the pure
//! projections its mappers delegate to. Today there is one group,
//! [`engines`]; the next groups (mail, components, observation) land as
//! siblings rather than as more methods in one file.
//!
//! Every tool here takes the design's *direct domain mail* path: it
//! constructs the fleet's own request kind, sends it to `FleetServer`, and
//! maps the typed reply into its declared output. Nothing synthesizes an
//! HTTP envelope, and nothing opens a loopback socket back into its own
//! chassis.

pub mod engines;
mod provider;

pub use provider::HubToolProvider;
