//! A candidate guest's held outbox (#7067).
//!
//! A republish builds each replacement guest while the old one is kept, and
//! only commits once every candidate is ready. Until then a candidate's mail
//! must not leave: [`ComponentCtx::hold_outbox`](super::ComponentCtx::hold_outbox),
//! called before `Component::instantiate`, makes its `init` and
//! `on_rehydrate` sends and replies land here in order instead.
//!
//! A send is held with no lineage stamped: a commit flushes it onto the
//! flushing turn's chain, so that chain settles only after the mail does. A
//! reply keeps its reply-table slot reserved, so nothing reallocates the slot
//! and a second answer to the handle is refused; a flush sends it on its
//! requester's chain and frees the slot, and a discard puts the slot's entry
//! and chain back for the old guest to answer.
//!
//! A discarded candidate's correlation ids and reply-lineage ids never leave
//! the engine, so the reinstated old guest may mint the same ids again
//! without any peer seeing one twice.

use std::cell::RefCell;
use std::thread;

use aether_data::{EngineId, SessionToken};

use super::ctx::RoutedSend;
use crate::actor::wasm::reply_table::{HeldChain, ReplyEntry};
use crate::mail::attachments::EncodedMail;
use crate::mail::{KindId, MailboxId};

/// A guest's answer to one reply handle, resolved against the handle's
/// entry and past the egress check, ready to send.
pub enum GuestAnswer {
    /// To a hub session, as a frame through the outbound.
    Session { token: SessionToken, kind_name: String, payload: Vec<u8>, correlation: u64 },
    /// To a local actor, as mail on the requester's chain.
    Component {
        recipient: MailboxId,
        kind: KindId,
        payload: EncodedMail,
        count: u32,
        correlation: u64,
        from: MailboxId,
    },
    /// To an actor on another engine, through the hub.
    Engine { engine_id: EngineId, mailbox_id: MailboxId, kind: KindId, payload: Vec<u8>, count: u32, correlation: u64 },
}

/// One mail a held candidate sent, in the order it sent it.
pub(super) enum HeldMail {
    /// A send or a detached send, with no lineage stamped yet.
    Send(RoutedSend),
    /// An answer to `handle`, whose slot is reserved: `entry` and `chain`
    /// are what the reservation moved out.
    Reply { handle: u32, entry: ReplyEntry, chain: Option<HeldChain>, answer: GuestAnswer },
}

/// The mail a candidate guest sent while its outbox is held. It must be
/// flushed or discarded: dropped with mail in it, it panics in debug builds
/// and logs an error in release, since the mail would be lost and each
/// reserved slot stranded.
///
/// The guard arms once the candidate is instantiated. Before that nothing
/// can flush or discard it, so a failed `init` drops its mail unarmed: the
/// candidate is refused, and no reply slot can be reserved while its reply
/// table is still empty.
#[derive(Default)]
pub(super) struct HeldOutbox {
    mail: RefCell<Vec<HeldMail>>,
    armed: bool,
}

impl HeldOutbox {
    pub(super) fn push(&self, mail: HeldMail) {
        self.mail.borrow_mut().push(mail);
    }

    pub(super) fn arm(&mut self) {
        self.armed = true;
    }

    /// Every held mail in send order, leaving the outbox empty.
    pub(super) fn drain(&mut self) -> Vec<HeldMail> {
        self.mail.take()
    }
}

impl Drop for HeldOutbox {
    fn drop(&mut self) {
        let held = self.mail.get_mut().len();
        if !self.armed || thread::panicking() {
            return;
        }
        debug_assert_eq!(held, 0, "a held guest outbox dropped with {held} mail unflushed and undiscarded");
        if held > 0 {
            tracing::error!(
                target: "aether_substrate::component",
                held,
                "a held guest outbox dropped unflushed and undiscarded; its mail is lost and its reserved reply slots stranded",
            );
        }
    }
}
