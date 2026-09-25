//! The chassis-root door to one composed root actor (ADR-0080 §6).
//!
//! Host code — a driver's loop, a passive embedder's pump, a render capture
//! hook — originates mail that no actor caused. Each such mail is a chassis
//! root, `MailId(CHASSIS_MAILBOX_ID, n)`, and every one is minted from the
//! engine's one counter on the [`Mailer`], so two senders never mint the same
//! root and the settlement registry never conflates two chains.

use std::sync::Arc;

use aether_actor::ActorRef;
use aether_data::{Kind, MailId};

use crate::chassis::inbox::SettlingInbox;
use crate::mail::Source;
use crate::mail::mailer::Mailer;

/// The chassis-root door to one composed root actor `R` (ADR-0080 §6). A driver
/// or passive embedder mints it once at boot and pushes host-originated mail to
/// `R` from its loop. It holds `R`'s proof, never a position, and reaches no
/// other actor. Every push mints its root from the engine's one counter.
pub struct RootPusher<R> {
    to: ActorRef<R>,
    mailer: Arc<Mailer>,
}

impl<R> RootPusher<R> {
    pub(crate) const fn new(to: ActorRef<R>, mailer: Arc<Mailer>) -> Self {
        Self { to, mailer }
    }

    /// Push `kind` to `R` as a fresh chassis root and return that root, for a
    /// caller that awaits its settlement. `reply_to`, when given, is an inbox
    /// the caller claimed and drains: `R`'s reply lands there.
    pub fn push_root<K: Kind>(&self, kind: &K, reply_to: Option<&SettlingInbox>) -> MailId {
        let minted = self.mailer.mint_chassis_root(self.to.id(), K::ID);
        let reply = reply_to.map_or(Source::NONE, |inbox| inbox.reply_source(minted.id().correlation_id));
        self.mailer.push_minted_root(minted, kind.encode_into_bytes(), reply)
    }
}
