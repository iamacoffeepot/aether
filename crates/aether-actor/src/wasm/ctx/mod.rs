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

use crate::model::CallerScope;

/// Authoritative raw lineage carries available to a wasm lifecycle or
/// receive context.
///
/// Kept separate from the routable mailbox id because mailbox tagging
/// overwrites the fold state's high nibble. The legacy guest ABI constructs
/// an unavailable value; scoped ABI siblings supply both carries.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawCallerScopes {
    current: Option<u64>,
    parent: Option<u64>,
}

impl RawCallerScopes {
    /// Raw scopes supplied by a scoped guest entrypoint.
    #[must_use]
    pub const fn available(current: u64, parent: u64) -> Self {
        Self { current: Some(current), parent: Some(parent) }
    }

    /// No raw scopes were supplied by the legacy guest entrypoint.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self { current: None, parent: None }
    }

    /// Raw scopes for a resolved target. `current` is its newly-produced
    /// carry; `parent` is the carry the resolver selected for non-root
    /// placement.
    #[must_use]
    pub(crate) const fn resolved(current: u64, parent: Option<u64>) -> Self {
        Self { current: Some(current), parent }
    }

    #[must_use]
    pub(crate) const fn from_options(current: Option<u64>, parent: Option<u64>) -> Self {
        Self { current, parent }
    }

    /// Select a scope for address resolution. Current/Parent absence is an
    /// explicit failure: a tagged route id is never fabricated as fallback.
    #[must_use]
    pub(crate) fn select(self, scope: CallerScope) -> u64 {
        self.try_select(scope).unwrap_or_else(|| match scope {
            CallerScope::Root => unreachable!("Root selection is always available"),
            CallerScope::Current => {
                panic!(
                    "raw Current caller scope unavailable; the legacy guest entrypoint did not supply lineage carries"
                )
            }
            CallerScope::Parent => {
                panic!(
                    "raw Parent caller scope unavailable; the legacy guest entrypoint did not supply lineage carries"
                )
            }
        })
    }

    /// Non-panicking selection used while propagating legacy-unavailable
    /// state into an inline child slot.
    #[must_use]
    pub(crate) const fn try_select(self, scope: CallerScope) -> Option<u64> {
        match scope {
            // Root-pinned resolvers ignore the value, so they remain usable
            // even when the legacy ABI supplied no lineage state.
            CallerScope::Root => Some(0),
            CallerScope::Current => self.current,
            CallerScope::Parent => self.parent,
        }
    }
}

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
