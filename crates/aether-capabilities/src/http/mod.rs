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

pub use kinds::*;

// Egress client surface (`client.rs`). `HttpConfig` is the always-on
// domain struct; the `Config`-derive `HttpConfigLayer` / `HttpOverlay`
// are native-only.
pub use client::{HttpCapability, HttpConfig};
#[cfg(feature = "native")]
pub use client::{HttpConfigLayer, HttpOverlay};

// Ingress server surface (`server.rs`). `HttpServerConfig` is the
// always-on domain struct; the `Config`-derive `HttpServerConfigLayer` /
// `HttpServerOverlay` and the bound-port `HttpServerHandle` are native-only.
#[cfg(feature = "native")]
pub use server::HttpServerConfigLayer;
#[cfg(not(target_arch = "wasm32"))]
pub use server::HttpServerHandle;
#[cfg(feature = "native")]
pub use server::HttpServerOverlay;
pub use server::{HttpServerCapability, HttpServerConfig};
