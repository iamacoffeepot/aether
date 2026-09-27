//! Issue 1958: `ctx.sender()` end-to-end fixture — the reading half.
//!
//! `on_source_query` (manual) handles `SourceQuery`, reads `ctx.sender()`,
//! and replies a `SourceReport` whose `had_sender` says whether it returned a
//! proof. The host routes the reply to the origin it stamped on the query,
//! the same origin `sender()` reads, so where the report lands is the other
//! half of the observation.
//!
//! Integration test pattern:
//! - Session case: the harness sends `SourceQuery` via `send_and_await_reply`;
//!   the reply carries `had_sender: false` (Session source → `None`).
//! - Component case: load this observer under its **default** name, then a
//!   [`SourceForwarder`](super::source_forwarder::SourceForwarder), which
//!   declares this actor as a dependency. The harness sends the fieldless
//!   `SendSourceQuery` (via `send_and_settle`) to the forwarder; the forwarder
//!   sends `SourceQuery` through its minted reference (component-origin mail);
//!   this actor reads `sender()` → `Some(forwarder)` and replies, so the
//!   report lands on the forwarder with `had_sender: true`. The forwarder logs
//!   the report's arrival, and the test reads that log with `log_tail` on the
//!   forwarder's address.

// `#[handler::manual]` and `#[handler]` methods take `&mut self` to match
// the dispatch ABI even when the actor carries no state.
#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, Erased, Manual, OutboundReply, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{SourceQuery, SourceReport};

pub struct SourceObserver;

#[actor]
impl WasmActor for SourceObserver {
    const NAMESPACE: &'static str = "test.source_observer";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(SourceObserver)
    }

    /// Read `sender()` from the inbound `SourceQuery` and reply whether it
    /// returned a proof. The reply goes to the origin the host stamped on the
    /// query, a component or a session alike.
    #[handler::manual]
    fn on_source_query(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, _query: SourceQuery) {
        ctx.reply(&SourceReport { had_sender: ctx.sender().is_some() });
    }
}
