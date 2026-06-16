//! Issue 1958: `source_mailbox()` end-to-end fixture.
//!
//! A single-actor module with two handlers:
//!
//! - `on_send_source_query` (auto): receives `SendSourceQuery { to }` and
//!   forwards a `SourceQuery` to `MailboxId(to)`, making this actor the
//!   component origin so the reader's `ctx.source_mailbox()` sees this
//!   actor's `MailboxId`.
//!
//! - `on_source_query` (manual): handles `SourceQuery`, reads
//!   `ctx.source_mailbox()`, logs it, broadcasts `SourceReport { mailbox_id }`
//!   to the test-bench observer mailbox, and replies it directly. `mailbox_id`
//!   is `0` when `source_mailbox()` returns `None` (Session / no-sender origin).
//!
//! Integration test pattern:
//! - Session case: the bench sends `SourceQuery` via `send_and_await`; the
//!   reply is `SourceReport { mailbox_id: 0 }` (Session source → None).
//! - Component case: load two instances ("sender" + "reader"). Bench sends
//!   `SendSourceQuery { to: reader_mailbox.0 }` (fire-and-settle) to sender.
//!   Sender forwards `SourceQuery` to reader (component-origin mail). Reader
//!   reads `source_mailbox()` → `Some(sender_mailbox)` → logs
//!   `"source_mailbox={sender_mailbox.0}"`. Test uses `log_tail` on the reader's
//!   address to verify the logged value equals `sender_mailbox.0`.

// `#[handler::manual]` and `#[handler]` methods take `&mut self` to match
// the dispatch ABI even when the actor carries no state.
#![allow(clippy::unused_self)]

use aether_actor::ffi::MAIL_BRIDGE;
use aether_actor::{
    BootError, FfiActor, FfiCtx, MailSender, Manual, OutboundReply, Resolver, actor,
};
use aether_data::Kind;
use aether_test_fixtures::{
    SendSourceQuery, SourceQuery, SourceReport, TEST_BENCH_OBSERVER_MAILBOX_NAME,
};

pub struct SourceObserver;

#[actor]
impl FfiActor for SourceObserver {
    const NAMESPACE: &'static str = "test.source_observer";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(SourceObserver)
    }

    /// Forward `SourceQuery` to the `MailboxId` named in `msg.to`, making
    /// *this* actor the component origin so the reader can recover our id
    /// via `ctx.source_mailbox()`. The target is a runtime-supplied `u64`
    /// (not a compile-time type), so we dispatch through the raw bridge
    /// rather than the typed `ctx.send::<R, K>` path.
    #[handler]
    fn on_send_source_query(&mut self, _ctx: &mut FfiCtx<'_>, msg: SendSourceQuery) {
        let bytes = SourceQuery.encode_into_bytes();
        // SAFETY: forwards to `raw::send_mail`. The `(ptr, len)` pair comes
        // from a valid `Vec<u8>` alive for the duration of this call; the
        // host copies before returning.
        MAIL_BRIDGE.send_mail(msg.to, SourceQuery::ID.0, &bytes, 1, false);
    }

    /// Read `source_mailbox()` from the inbound `SourceQuery`, log the value
    /// (so `log_tail` can retrieve the exact raw id in the integration test),
    /// broadcast `SourceReport { mailbox_id }` to the observer, and reply to
    /// the direct sender with the same report.
    #[handler::manual]
    fn on_source_query(&mut self, ctx: &mut FfiCtx<'_, Manual>, _query: SourceQuery) {
        let mailbox_id = ctx.source_mailbox().map_or(0, |m| m.0);
        // Log the raw value so the TestBench integration test can verify it
        // with `log_tail` without relying on broadcast payload access.
        tracing::info!(target: "test.source_observer", "source_mailbox={mailbox_id}");
        // Broadcast to the observer for count-based assertions.
        ctx.send_to_named::<SourceReport>(
            TEST_BENCH_OBSERVER_MAILBOX_NAME,
            &SourceReport { mailbox_id },
        );
        // Reply to the bench when it sent `SourceQuery` directly (Session case).
        ctx.reply(&SourceReport { mailbox_id });
    }
}

aether_actor::export!(SourceObserver);
