//! Issue 1958: `ctx.sender()` end-to-end fixture — the reading half.
//!
//! `on_source_query` (manual) handles `SourceQuery`, reads
//! `ctx.sender()`, logs its id, and answers the query: through
//! the proven sender when there is one, else by reply. `mailbox_id` is `0`
//! when `sender()` returns `None` (Session / no-sender origin).
//!
//! Integration test pattern:
//! - Session case: the harness sends `SourceQuery` via `send_and_await_reply`; the
//!   reply is `SourceReport { mailbox_id: 0 }` (Session source → None).
//! - Component case: load this observer under its **default** name, then a
//!   [`SourceForwarder`](super::source_forwarder::SourceForwarder), which
//!   declares this actor as a dependency. The harness sends the fieldless
//!   `SendSourceQuery` (via `send_and_settle`) to the forwarder; the forwarder
//!   sends `SourceQuery` through its minted reference (component-origin mail);
//!   this actor reads `sender()` → `Some(forwarder)` and sends its report
//!   through that reference. The forwarder logs the report's arrival, and the
//!   test reads that log with `log_tail` on the forwarder's address.

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

    /// Read `sender()` from the inbound `SourceQuery`, log its id, and answer:
    /// a component sender gets the report sent through its proven reference,
    /// and a session sender (no reference) gets it as a reply.
    #[handler::manual]
    fn on_source_query(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, _query: SourceQuery) {
        let sender = ctx.sender();
        let mailbox_id = sender.map_or(0, |sender| sender.id().0);
        tracing::info!(target: "test.source_observer", "source_mailbox={mailbox_id}");
        match sender {
            Some(sender) => ctx.send_to(sender, &SourceReport { mailbox_id }),
            None => ctx.reply(&SourceReport { mailbox_id }),
        }
    }
}
