//! The two HTTP capabilities, co-located (ADR-0121): the egress client
//! ([`client`], the `aether.http` egress cap) and the ingress server
//! ([`server`], the `aether.http.server` cap). They stay two distinct
//! capabilities — separate cap structs, separate `NAMESPACE` / mailboxes —
//! sharing one crate and one [`kinds`] module, the wire vocabulary both
//! own (ADR-0121). The substrate core dispatches none of the HTTP kinds,
//! so they live with the capabilities rather than in `aether-kinds`.
//!
//! ## Crate shape
//!
//! Extracted from `aether-capabilities` (iamacoffeepot/aether#3758) as the
//! final per-cap crate of the arc that dissolves the capabilities monolith.
//! Both caps move as one unit because they are one wire vocabulary split
//! across two directions — [`kinds`] is shared, and the typed route surface
//! ([`typed`], [`stream`]) is authored against the server while the client
//! is the egress side of the same `aether.http` family.
//!
//! It depends downward on `aether-component` because `defer` names
//! `ComponentHostCapability` to resolve the handler component a deferred
//! request is answered through (ADR-0154).
//!
//! The ADR-0122 identity/runtime split rides the `runtime` feature: the mail
//! kinds, the wasm-safe cap identities with their `HandlesKind` markers, the
//! typed route surface, and the stream handles compile always-on, so a
//! marker-only wasm guest can address `ctx.actor::<HttpCapability>()` and
//! author `#[http::router]` routes without dragging the substrate through.
//! The `aether_substrate`-typed half (both cap states, the listener and its
//! dispatch shards, the deferred-reply obligation table) is gated behind it.

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

// ADR-0154 §2/§3 native deferred-reply machinery for the typed route
// surface (`Ctx::defer`, the reply-obligation table). Native-only — the
// obligation hold is `InboundMail` — so behind the `runtime` feature.
#[cfg(feature = "runtime")]
mod defer;

pub use kinds::*;

// ADR-0131 typed route-authoring surface. The `#[router]` / `#[route]`
// proc macros (host-compiled, so wasm-safe) re-exported from the
// cap-owned derive crate, alongside the runtime types they compile down
// to — consumers write `#[http::router]` / `#[http::route]` next to
// `http::FromRequest` / `http::Ctx` / `http::Route`.
pub use aether_http_derive::{reply, route, router};
pub use typed::{Ctx, FromPathSegment, FromRequest, Outcome, Path, Route};

// Deferred-route glue helpers the `#[http::route]` / `#[http::reply]` macros
// emit calls to (ADR-0154): `answer_deferred` (answer a held request from its
// downstream reply, recovering the requester via `take_context`) and
// `answer_now` (the synchronous `Outcome::Reply` arm). Re-exported here so the
// macro emits one `::aether_http::…` path a consumer resolves
// through its existing dependency. Runtime-only — `reply_to` is native.
#[cfg(feature = "runtime")]
pub use defer::{Peer, answer_deferred, answer_now};

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
