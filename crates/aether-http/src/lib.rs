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
//! states, the listener and its dispatch shards, and deferred routes. The kinds, cap identities, typed routes, and stream
//! handles compile always-on, so a wasm guest can send
//! `ctx.send::<HttpCapability>(..)` and author routes without pulling the
//! substrate through (ADR-0122).

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

// ADR-0154 §2 native deferred routes for the typed route surface
// (`Ctx::defer`, `Outcome`, the held-reply glue helpers). Native-only — the
// held reply is `NativeCtx::hold`'s — so behind the `runtime` feature.
#[cfg(feature = "runtime")]
mod defer;

pub use kinds::*;

// ADR-0131 typed route-authoring surface. The `#[router]` / `#[route]`
// proc macros (host-compiled, so wasm-safe) re-exported from the
// cap-owned derive crate, alongside the runtime types they compile down
// to — consumers write `#[http::router]` / `#[http::route]` next to
// `http::FromRequest` / `http::Ctx` / `http::Route`.
pub use aether_http_derive::{reply, route, router};
pub use typed::{Ctx, FromPathSegment, FromRequest, Path, Route, route_matches, route_rank};

// ADR-0154 deferred routes over ADR-0243 held replies: `Outcome` is what a
// route that may defer returns, and the glue helpers the `#[http::router]` /
// `#[http::reply]` macros emit calls to answer through the router's held
// reply. `Pending` is re-exported so a router with a deferred route returns
// `::aether_http::Pending<HttpRouterReply>`, a path whose last segment
// `#[actor]` reads as a held reply. Re-exported here so the macros emit one
// `::aether_http::…` path a consumer resolves through its existing
// dependency. Runtime-only — the held reply is native.
#[cfg(feature = "runtime")]
pub use aether_substrate::actor::native::Pending;
#[cfg(feature = "runtime")]
pub use defer::{Deferred, DeferredRequest, Outcome, answer_deferred, answer_now};

// ADR-0133 reply-based data-phase stream handles. Wasm-safe like `typed`,
// so a `default-features = false` guest that streams gets them without the
// native runtime.
pub use stream::{RequestStream, ResponseStream, WebSocketStream};

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
