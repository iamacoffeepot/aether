//! Test-only helpers shared by the crate's table tests.

use aether_actor::wasm::inline::Registry;
use aether_actor::{AnyActorRef, WasmCtx};

/// The position the synthetic ctx dispatches for. Only its distinctness from
/// the ids the tests prove matters.
const SHELL_MAILBOX: u64 = 0x5E11;

/// A proof for position `id`, minted the one way a guest can mint one: from
/// the dispatch source the host threaded, lifted by `ctx.sender()`. There is no
/// constructor to reach for instead — `AnyActorRef::new` is private to
/// `aether-actor` — and that closed door is exactly why the tables can hold
/// proofs rather than positions.
pub fn proven(id: u64) -> AnyActorRef {
    let registry = Registry::new();
    WasmCtx::__new(SHELL_MAILBOX, &registry, id).sender().expect("a threaded dispatch source mints a proof")
}
