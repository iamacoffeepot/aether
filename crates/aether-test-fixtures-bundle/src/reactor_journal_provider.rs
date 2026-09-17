//! Controlled journal reader for real-WASM reactor interleaving scenarios.

use aether_actor::{ActorInitError, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_bloomery_kinds::{JournalEntry, ReadEvents, ReadEventsResult};
use aether_test_fixtures_kinds::{
    ReactorJournalConfig, ReactorJournalStatus, ReactorJournalStatusQuery, ReleaseReactorJournalPage,
};

/// A read actor that parks one reply until a test explicitly releases it.
pub struct ReactorJournalProvider {
    head: u64,
    entries: Vec<JournalEntry>,
    pending: Option<(ReplyHandle, ReadEvents)>,
}

#[actor]
impl WasmActor for ReactorJournalProvider {
    type Config = ReactorJournalConfig;
    const NAMESPACE: &'static str = "test.bloomery.reactor.journal_provider";

    fn init(config: ReactorJournalConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { head: config.head, entries: config.entries, pending: None })
    }

    #[handler::manual]
    fn on_read(&mut self, ctx: &mut WasmCtx<'_, Manual>, request: ReadEvents) {
        if let Some(reply) = ctx.reply_target() {
            if self.pending.is_some() {
                ctx.reply_to(
                    reply,
                    &ReadEventsResult::Err {
                        after: request.after,
                        message: "controlled provider already has a pending read".into(),
                    },
                );
            } else {
                self.pending = Some((reply, request));
            }
        }
    }

    #[handler::manual]
    fn on_status(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: ReactorJournalStatusQuery) {
        if let Some((_, request)) = &self.pending {
            ctx.reply(&ReactorJournalStatus { pending: true, after: request.after, limit: request.limit });
        } else {
            ctx.reply(&ReactorJournalStatus { pending: false, after: 0, limit: 0 });
        }
    }

    #[handler::manual]
    fn on_release(&mut self, ctx: &mut WasmCtx<'_, Manual>, mode: ReleaseReactorJournalPage) {
        let Some((reply, request)) = self.pending.take() else {
            return;
        };
        if matches!(&mode, ReleaseReactorJournalPage::BackendError) {
            ctx.reply_to(
                reply,
                &ReadEventsResult::Err { after: request.after, message: "fixture backend failure".into() },
            );
            return;
        }
        let mut entries: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| entry.seq > request.after)
            .take(request.limit as usize)
            .cloned()
            .collect();
        let mut after = request.after;
        let mut head = self.head;
        match mode {
            ReleaseReactorJournalPage::Exact | ReleaseReactorJournalPage::BackendError => {}
            ReleaseReactorJournalPage::WrongAfter => after = after.saturating_add(1),
            ReleaseReactorJournalPage::Short => {
                entries.pop();
            }
            ReleaseReactorJournalPage::WrongSequence => {
                if let Some(first) = entries.first_mut() {
                    first.seq = first.seq.saturating_add(1);
                }
            }
            ReleaseReactorJournalPage::LowHead => head = 0,
        }
        ctx.reply_to(reply, &ReadEventsResult::Ok { after, head, entries });
    }
}
