//! The per-actor send-side buffer (ADR-0087) and the buffered sends that fill
//! it: payload into the ring in place, route metadata into the window.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aether_kinds::trace::Nanos;

use super::NativeBinding;
use super::pending::{ComponentOrigin, PendingBirthWork, PendingMail, PendingOwnerBatchWork, PendingPayload};
use crate::mail::registry::effect::{PreparedSpawnCommit, RegistryBatch, RegistryBatchResult};
use crate::mail::ring::{MailRing, RingFull};
use crate::mail::{MailId, Source, SourceAddr};

/// Per-actor outbound ring capacity (ADR-0087). Sized to hold a typical
/// handler's small-mail fan-out as one blob; a mail that doesn't fit (a
/// large payload, or a very wide fan-out that fills the ring) degrades to
/// the [`MailRef::Owned`] copy-out valve in
/// [`NativeBinding::flush_outbound`] / `push_envelope_buffered` rather
/// than blocking — the large-payload zero-copy path is the deferred fork
/// on iamacoffeepot/aether#1101.
pub(super) const ACTOR_RING_BYTES: usize = 64 * 1024;

/// Per-actor send-side buffer that builds blobs **in place** (2c,
/// iamacoffeepot/aether#1110). `push_envelope_buffered` writes each send
/// straight into the ring as it happens — the blob is opened lazily on
/// the first send of a flush window — and records only route metadata
/// here. `flush_outbound` seals the blob and routes. There is no payload
/// staging buffer: the bytes land in the ring exactly once (the only
/// copy is out of the caller's slice, which is unavoidable since it is
/// not stable past the call).
///
/// `mails` is **reused** across windows (cleared, not freed). `ring` is
/// lazily created on the first buffered send, so actors that never buffer
/// (wasm trampolines, inline-only caps) pay no ring allocation.
pub(super) struct OutboundBuffer {
    /// A staged activation keeps buffered lifecycle effects local until the
    /// registry owner has promoted its route to `Live`. The private activation
    /// barrier bypasses this buffer; ordinary `NativeCtx` drops remain harmless
    /// while the hold is set.
    pub(super) activation_held: bool,
    /// Lazily created on the first buffered send. `Arc` so each minted
    /// [`MailRef::InRing`] carries the ring's lifetime by refcount.
    pub(super) ring: Option<Arc<MailRing>>,
    /// Whether a ring blob is currently open — between the first send of
    /// a flush window and the flush's `seal`.
    pub(super) blob_open: bool,
    /// iamacoffeepot/aether#1158: the instant this outbound window
    /// **opened** — stamped by its first buffered native or component send,
    /// shared by every mail in the window. The
    /// flush reads it as each deferred `Sent`'s `t_construct_start`
    /// (falling back to `flush_begin` if somehow unset) so `t_sent −
    /// t_construct_start` is the **construct** span, and resets it to
    /// `None` after draining so the next window re-stamps.
    pub(super) construct_start: Option<Nanos>,
    /// Per-mail route metadata for the current flush window.
    pub(super) mails: Vec<PendingMail>,
    /// The guest-authored origins the activation hold admitted into this
    /// window. Only a wasm trampoline's staged activation fills it, so it
    /// remains unallocated on every ordinary handler flush — and its emptiness,
    /// not a per-mail tag, is the whole route test the native flush pays.
    pub(super) component_origins: Vec<ComponentOrigin>,
    /// Births are rare; this vector remains unallocated on the ordinary
    /// mail-only handler path.
    pub(super) births: Vec<PendingBirthWork>,
    /// Handler-staged registry batches are uncommon and stay unallocated on
    /// the ordinary mail-only path.
    pub(super) owner_batches: Vec<PendingOwnerBatchWork>,
}

impl OutboundBuffer {
    pub(super) fn new() -> Self {
        Self {
            activation_held: false,
            ring: None,
            blob_open: false,
            construct_start: None,
            mails: Vec::new(),
            component_origins: Vec::new(),
            births: Vec::new(),
            owner_batches: Vec::new(),
        }
    }
}

impl NativeBinding {
    /// ADR-0087 / 2b: the buffering counterpart to
    /// [`Self::push_envelope_returning_root`], used by the per-handler
    /// send surface ([`super::ctx::NativeCtx`](crate::actor::native::ctx::NativeCtx) /
    /// [`super::mailbox::NativeActorMailbox`](crate::actor::native::mailbox::NativeActorMailbox)). Rather than allocating an
    /// owned `Vec` and routing immediately, it copies the bytes into the
    /// reused per-actor scratch arena and records the route
    /// metadata; [`Self::flush_outbound`] forms the blob and routes at
    /// handler end.
    ///
    /// The settlement-counter increment stays **eager** (fired here, at
    /// send time, not at flush) so the chain's `in_flight` is exact and
    /// settlement (ADR-0082) never settles early. The `Sent` *trace*
    /// event, by contrast, is deferred to [`Self::flush_outbound`] and
    /// stamped with the frame-level flush-begin instant
    /// (iamacoffeepot/aether#1150) — anchoring it there instead of this
    /// smeared per-send call site, which otherwise absorbs the rest of
    /// the handler that ran after the send. Returns the minted `MailId`
    /// (== the new root when `inherited_root.is_none()`) exactly like the
    /// eager variant, so settlement subscription works unchanged.
    ///
    /// # Panics
    /// Panics if the outbound-buffer mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub fn push_envelope_buffered(
        &self,
        recipient: u64,
        kind: u64,
        bytes: &[u8],
        count: u32,
        parent_mail: Option<MailId>,
        inherited_root: Option<MailId>,
    ) -> MailId {
        self.push_envelope_buffered_with_reply_to(recipient, kind, bytes, count, parent_mail, inherited_root, None)
    }

    /// Re-dispatcher variant of [`Self::push_envelope_buffered`] that
    /// accepts an explicit `reply_to` instead of stamping the default
    /// `Source::with_correlation(SourceAddr::Component(self_mailbox),
    /// auto_correlation)`. The minted [`MailId`] and the `in_flight`
    /// settlement increment are unaffected — they still use this
    /// actor's correlation counter — only the recipient's
    /// `OutboundReply::reply_target()` view changes.
    ///
    /// Used by re-dispatch caps (today: `TraceDispatchCapability`
    /// servicing `DispatchTraced`) that forward someone else's call:
    /// the children's deferred replies must bubble up to the original
    /// caller (e.g. the RPC server holding the wire `cid`'s in-flight
    /// entry), not get stranded at the re-dispatcher's mailbox where
    /// no handler exists for them.
    ///
    /// `reply_to_override = None` is the same shape as
    /// [`Self::push_envelope_buffered`].
    ///
    /// # Panics
    /// Panics if the outbound-buffer mutex is poisoned — fail-fast per
    /// ADR-0063.
    #[allow(
        clippy::too_many_arguments,
        reason = "re-dispatch variant adds reply_to_override to the existing 6-arg shape; \
                  splitting would force callers through two separate code paths"
    )]
    pub fn push_envelope_buffered_with_reply_to(
        &self,
        recipient: u64,
        kind: u64,
        bytes: &[u8],
        count: u32,
        parent_mail: Option<MailId>,
        inherited_root: Option<MailId>,
        reply_to_override: Option<Source>,
    ) -> MailId {
        let correlation = self.correlation.fetch_add(1, Ordering::AcqRel) + 1;
        let reply_to = reply_to_override
            .unwrap_or_else(|| Source::with_correlation(SourceAddr::Component(self.self_mailbox()), correlation));
        let mail_id = MailId::new(self.self_mailbox(), correlation);
        let root = inherited_root.unwrap_or(mail_id);
        // iamacoffeepot/aether#1150: only the settlement increment is
        // eager here; the `Sent` trace event emits at flush against the
        // flush-begin anchor (see `flush_outbound_inner`). The recipient
        // id, kind, and lineage ride the `PendingMail` to flush, where the
        // deferred `Sent` is built from the routed `Mail`.
        self.mailer.record_sent_inflight(root);
        let mut buf = self.outbound.lock().expect("outbound buffer poisoned; fail-fast per ADR-0063");
        // Write the payload into the ring in place. Open the blob lazily
        // on the first send of this flush window; on `RingFull` (full ring
        // or oversized payload) copy out to `Owned` — the never-block
        // valve. The open blob is left intact on `RingFull`, so a later
        // send (after a consumer frees space) can still extend it.
        let payload = {
            let OutboundBuffer { ring, blob_open, construct_start, .. } = &mut *buf;
            let ring = ring.get_or_insert_with(|| Arc::new(MailRing::with_capacity(ACTOR_RING_BYTES)));
            if !*blob_open {
                ring.open_blob();
                *blob_open = true;
                // iamacoffeepot/aether#1158: the blob just opened — stamp
                // the construct-start instant shared by every mail in this
                // flush window. `t_sent − t_construct_start` (flush-begin −
                // this) is the **construct** span (the producer building
                // the blob).
                if construct_start.is_none() {
                    *construct_start = Some(self.mailer.now_nanos());
                }
            }
            match ring.append(recipient, kind, bytes) {
                Ok(loc) => PendingPayload::InRing(loc),
                Err(RingFull) => PendingPayload::Owned(bytes.to_vec()),
            }
        };
        buf.mails.push(PendingMail { recipient, kind, payload, count, reply_to, mail_id, root, parent_mail });
        mail_id
    }

    /// Append one prepared child birth at its exact declaration point in the
    /// current handler's outbound work. The ordinary no-spawn path pays only
    /// the empty-vector check at flush.
    pub(crate) fn stage_child_birth(&self, commit: PreparedSpawnCommit) {
        let mut buffer = self.outbound.lock().expect("outbound buffer poisoned; fail-fast per ADR-0063");
        let after_mail = buffer.mails.len();
        let recipient = commit.route.id;
        buffer.births.push(PendingBirthWork { after_mail, recipient, commit });
    }

    /// Append one typed registry batch to the handler's ordered outbound work.
    /// Submission happens at flush through the reserved owner-batch path.
    pub(crate) fn stage_owner_batch(
        &self,
        batch: RegistryBatch,
        completion: super::offload::blocking::DeferredCompletion<RegistryBatchResult>,
    ) {
        self.outbound
            .lock()
            .expect("outbound buffer poisoned; fail-fast per ADR-0063")
            .owner_batches
            .push(PendingOwnerBatchWork { batch, completion });
    }
}
