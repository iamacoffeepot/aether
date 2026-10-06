//! Spike (log-stream-scale): the verbs the gatherer capability and the
//! benchmark's producers reach the log tap and their own mailbox through.

use std::sync::Arc;

use aether_actor::ReplyMode;
use aether_data::{ErasedActorPath, Kind, MailboxId};

use crate::actor::native::binding::OutboundSend;
use crate::mail::attachments::encode_envelope;
use crate::mail::log_tap::LogTap;

use super::NativeCtx;

impl<M: ReplyMode, A> NativeCtx<'_, A, M> {
    /// Open the engine's log tap with this actor as its sink.
    pub fn log_tap_open(&self, mode: u8, backfill: bool, buffer_cap: usize, shards: usize) {
        self.binding.mailer().log_tap().open(mode, self.binding.self_mailbox(), backfill, buffer_cap, shards);
    }

    /// Close the engine's log tap.
    pub fn log_tap_close(&self) {
        self.binding.mailer().log_tap().close();
    }

    /// The engine's log tap, for the sink's per-tick swap and its counters.
    pub fn log_tap(&self) -> Arc<LogTap> {
        Arc::clone(self.binding.mailer().log_tap())
    }

    /// The canonical path of the actor at `position`, or `None` when the
    /// registry holds no route record for it.
    pub fn log_tap_path(&self, position: MailboxId) -> Option<ErasedActorPath> {
        let mailer = self.binding.mailer();
        mailer.stamped_sender(position).map(|reference| mailer.actor_path(reference))
    }

    /// Mail `payload` to this actor's own mailbox: under the running chain, or
    /// on a fresh one when `detached`.
    pub fn spike_send_self<K: Kind>(&mut self, payload: &K, detached: bool) {
        let encoded = encode_envelope(self.binding.mailer().blob_store(), payload);
        let lineage = (!detached).then(|| (self.outbound_parent(), self.outbound_root()));
        let (parent_mail, inherited_root) = lineage.unwrap_or((None, None));
        self.binding.push_envelope_buffered(OutboundSend {
            recipient: self.binding.self_mailbox().0,
            kind: K::ID.0,
            bytes: &encoded.bytes,
            attachments: encoded.attachments.as_deref().unwrap_or_default(),
            count: 1,
            parent_mail,
            inherited_root,
        });
    }
}
