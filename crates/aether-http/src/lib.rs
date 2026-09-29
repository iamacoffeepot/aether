//! The two HTTP capabilities: the egress client ([`client`], on the
//! `aether.http` mailbox) and the ingress server ([`server`], on
//! `aether.http.server`). They are separate capabilities with separate cap
//! structs and mailboxes, sharing one crate and one [`kinds`] module, the wire
//! vocabulary both own. The substrate core dispatches none of the HTTP kinds,
//! so they live here and not in `aether-kinds`.
//!
//! [`typed`] and [`stream`] are the route-authoring surface, written against
//! the server; the `#[http::router]` and `#[http::route]` macros come from
//! `aether-http-derive` and are re-exported here so a route sits next to the
//! `FromRequest` / `Path` / `Ctx` types this crate owns.
//!
//! The `runtime` feature carries the `aether_substrate`-typed half: both cap
//! states and the listener and its dispatch shards. The kinds, cap
//! identities, typed routes, and stream handles compile always-on, so a wasm
//! guest can send `ctx.send::<HttpCapability>(..)` and author routes without
//! pulling the substrate through (ADR-0122).

#![forbid(unsafe_code)]

// ADR-0131: self-alias so the `#[http::router]` macro's emitted
// `::aether_http::…` paths resolve inside this crate's own route fixtures
// (the pattern `aether-actor` / `aether-substrate` already use for their
// derive-emitted paths).
extern crate self as aether_http;

pub mod client;
pub mod kinds;
pub mod server;
pub mod stream;
pub mod typed;

pub use kinds::*;

// ADR-0131 typed route-authoring surface. The `#[router]` / `#[route]`
// proc macros (host-compiled, so wasm-safe) re-exported from the
// cap-owned derive crate, alongside the runtime types they compile down
// to — consumers write `#[http::router]` / `#[http::route]` next to
// `http::FromRequest` / `http::Ctx` / `http::Route`.
pub use aether_http_derive::{route, router};
pub use typed::{Ctx, FromPathSegment, FromRequest, Path, Route, route_matches, route_rank};

// ADR-0133 reply-based data-phase stream handles. Wasm-safe like `typed`,
// so a `default-features = false` guest that streams gets them without the
// native runtime.
pub use stream::{RequestCreditSink, RequestStream, ResponseSink, ResponseStream, WebSocketSink, WebSocketStream};

// Egress client surface (`client.rs`). `HttpConfig` is the always-on
// domain struct; the `Config`-derive `HttpConfigLayer` / `HttpOverlay`
// are native-only.
pub use client::{HttpCapability, HttpConfig};
#[cfg(feature = "runtime")]
pub use client::{HttpConfigLayer, HttpOverlay};

// Ingress server surface (`server.rs`). `HttpServerConfig` is the
// always-on domain struct; the `Config`-derive `HttpServerConfigLayer` /
// `HttpServerOverlay` and the bound-port `HttpServerHandle` are native-only.
#[cfg(feature = "runtime")]
pub use server::HttpServerConfigLayer;
#[cfg(not(target_family = "wasm"))]
pub use server::HttpServerHandle;
#[cfg(feature = "runtime")]
pub use server::HttpServerOverlay;
pub use server::{HttpServerCapability, HttpServerConfig};
