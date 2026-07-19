//! The two HTTP capabilities, co-located (ADR-0121): the egress client
//! (`client.rs`, the `aether.http` egress cap) and the ingress server
//! (`server.rs`, the `aether.http.server` cap). They stay two distinct
//! capabilities — separate cap structs, separate `NAMESPACE` / mailboxes —
//! sharing one parent module and one `kinds.rs`, the wire vocabulary both
//! own (ADR-0121). The substrate core dispatches none of the HTTP kinds,
//! so they live with the capabilities rather than in `aether-kinds`.

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
pub use aether_capabilities_derive::{route, router};
pub use typed::{Ctx, FromPathSegment, FromRequest, Path, Route};

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
