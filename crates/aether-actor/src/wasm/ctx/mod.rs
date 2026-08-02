// Wire-encode: `usize → u32` narrowings forward `(ptr, len)` pairs
// to the wasm32 host-fn ABI (`_p32` convention, ADR-0024).
#![allow(clippy::cast_possible_truncation)]

//! Concrete wasm ctx structs — [`WasmInitCtx`] / [`WasmCtx`] / [`WasmDropCtx`].
//!
//! The ctx interface is spelled by the per-stage capability traits in
//! [`crate::model::ctx`]; these structs are concrete impls that route
//! outbound calls through the per-concern bridge functions in
//! `crate::wasm::bridge::mail` and `crate::wasm::bridge::persist`.
//! Ctxs hold per-mail state only (mailbox id at init; reply target at
//! receive), and dispatch goes through the bridge functions directly.
//!
//! The submodules follow the lifecycle a component author meets in order —
//! `init`, `wire`, `receive`, `drop` — plus the three cross-cutting surfaces
//! the receive ctx carries and that are large enough to name in their own
//! right: `send` (its outbound mail surface), `relative` (cluster-relative
//! addressing) and `spawn` (detached and inline child creation).

mod drop;
mod init;
mod receive;
mod relative;
mod send;
mod spawn;
mod wire;

#[cfg(test)]
mod tests;

pub use drop::WasmDropCtx;
pub use init::WasmInitCtx;
pub use receive::{NO_INBOUND_SOURCE, WasmCtx};
pub use relative::RelativeMailbox;
pub use spawn::{ActorTypeTag, SpawnError};
pub use wire::WireCtx;

pub(crate) use drop::CapturedState;
pub(crate) use spawn::install_inline_child;
