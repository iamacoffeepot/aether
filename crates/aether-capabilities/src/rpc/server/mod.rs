//! `aether.rpc.server` — generic TCP RPC server capability (issue 750).
//!
//! Singleton actor. Binds a `TcpListener` on the configured addr at
//! init, runs a sidecar accept thread that spawns one reader thread
//! per accepted connection. Reader threads read
//! length-prefix frames via [`aether_codec::frame`] and push them
//! through an internal mpsc; an `RpcInboundReady` wake mail tells the
//! cap's dispatcher to drain.
//!
//! On `Call`, the cap dispatches the wire-borne envelope via
//! `NativeCtx::send_envelope_as_root` (fresh causal chain — the wake
//! mail is causally unrelated to the wire-borne Call) and subscribes
//! to settlement of the resulting root via
//! `SettlementRegistry::subscribe_settlement_mail`. Any reply mail
//! addressed back at this cap with the dispatch's correlation id
//! gets lifted into a `ReplyEvent` and written to the originating
//! connection; the settlement notice closes the call with a
//! `ReplyEnd`.

// Handler-signature kinds need to be importable at file root for the
// `#[actor]`-emitted `HandlesKind<K>` markers (always-on against the
// identity, ADR-0122). `RpcInboundReady` is the cap's own wake-mail kind
// (ADR-0121); `Settled` stays in `aether-kinds`.
use crate::rpc::kinds::RpcInboundReady;
use aether_kinds::trace::Settled;

// Re-export the cap's config at file root for chassis builders. The
// `RpcServerConfig` type names no `aether_substrate` type, so it stays a
// top-level `not(wasm32)` plain struct; the `RpcServerHandle` boot artifact
// lives in the runtime half and is re-exported below under the runtime
// gate.
#[cfg(not(target_family = "wasm"))]
mod config;
#[cfg(not(target_family = "wasm"))]
pub use config::RpcServerConfig;
#[cfg(feature = "runtime")]
pub use runtime::RpcServerHandle;

// Named at file root so the runtime half reaches it through `super::`
// (`RpcServerState` stores `peer_kind: PeerKind`).
use aether_rpc::rpc::PeerKind;

// The standalone connection plumbing (sidecar event type, per-connection
// state, reader loop, oversize guard) lives in `connection`; the runtime
// half `use`s it. Native-only — it owns a `TcpStream` + OS threads, elided
// on the wasm marker build.
#[cfg(not(target_family = "wasm"))]
mod connection;

#[cfg(test)]
mod tests;

/// `aether.rpc.server` cap **identity** (ADR-0122 identity/runtime split,
/// ADR-0123 struct-hosted form). A ZST carrying only the addressing —
/// `Addressable` (`NAMESPACE`, `Resolver`), the per-handler `HandlesKind`
/// markers, and the name-inventory entry, all emitted always-on by
/// `#[actor]` from the runtime `impl NativeActor`. The state-bearing runtime
/// (`RpcServerState`, which owns the TCP listener bookkeeping and
/// per-connection state) plus the handler bodies live in `runtime.rs` behind
/// the one `feature = "runtime"` gate, so a transport-only build never names
/// `RpcServerState` nor pulls `aether_substrate` through this cap.
#[actor(singleton)]
pub struct RpcServerCapability;

// The struct-hosted `#[actor(singleton)]` reads the sibling `runtime` module
// off disk, lifts the `NAMESPACE` + `#[handler]` kinds out of the
// `#[runtime] impl NativeActor` there, and emits the always-on identity
// markers (`Addressable`, one `HandlesKind<K>` per handler, the
// name-inventory entry) against this struct. The kind types those markers
// name (`RpcInboundReady` / `Settled`) are imported at file root above.
use aether_actor::actor;

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `RpcServerState`, `InFlight`, the `RpcServerHandle` boot artifact, the
// `#[runtime] impl NativeActor` with the handler bodies, the per-connection
// helper methods) — lives in `runtime.rs`, gated once here.
#[cfg(feature = "runtime")]
mod runtime;
