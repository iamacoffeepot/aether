//! Issue 1958: `ctx.sender()` end-to-end fixture — the reading half.
//!
//! `on_source_query` (manual) handles `SourceQuery`, reads
//! `ctx.sender()`, logs its id, broadcasts `SourceReport { mailbox_id }`
//! to the substrate-harness observer mailbox, and replies it directly.
//! `mailbox_id` is `0` when `sender()` returns `None` (Session /
//! no-sender origin).
//!
//! Integration test pattern:
//! - Session case: the harness sends `SourceQuery` via `send_and_await_reply`; the
//!   reply is `SourceReport { mailbox_id: 0 }` (Session source → None).
//! - Component case: load this observer under its **default** name, then a
//!   [`SourceForwarder`](super::source_forwarder::SourceForwarder), which
//!   declares this actor as a dependency. The harness sends the fieldless
//!   `SendSourceQuery` (via `send_and_settle`) to the forwarder; the forwarder
//!   sends `SourceQuery` through its minted reference (component-origin mail);
//!   this actor reads `sender()` → `Some(forwarder_mailbox)` → logs
//!   `"source_mailbox={forwarder_mailbox.0}"`. The test uses `log_tail` on this
//!   actor's address to verify the logged value equals the forwarder's id.

// `#[handler::manual]` and `#[handler]` methods take `&mut self` to match
// the dispatch ABI even when the actor carries no state.
#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, Erased, MailSender, Manual, OutboundReply, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME, SourceQuery, SourceReport};

pub struct SourceObserver;

#[actor]
impl WasmActor for SourceObserver {
    const NAMESPACE: &'static str = "test.source_observer";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(SourceObserver)
    }

    /// Read `sender()` from the inbound `SourceQuery`, log its id
    /// (so `log_tail` can retrieve the exact raw id in the integration test),
    /// broadcast `SourceReport { mailbox_id }` to the observer, and reply to
    /// the direct sender with the same report.
    #[handler::manual]
    fn on_source_query(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, _query: SourceQuery) {
        let mailbox_id = ctx.sender().map_or(0, |sender| sender.id().0);
        // Log the raw value so the SubstrateHarness integration test can verify it
        // with `log_tail` without relying on broadcast payload access.
        tracing::info!(target: "test.source_observer", "source_mailbox={mailbox_id}");
        // Broadcast to the observer for count-based assertions.
        ctx.send_to_named::<SourceReport>(SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME, &SourceReport { mailbox_id });
        // Reply to the harness when it sent `SourceQuery` directly (Session case).
        ctx.reply(&SourceReport { mailbox_id });
    }
}
