//! The rows a republish drives one guest through (ADR-0241 §7): prepare a
//! candidate beside the running guest, then commit it or abort it. The
//! component host sends them over its private `GuestControl` protocol; no
//! other actor holds that proof.

use aether_actor::HeldReply;
use aether_data::Blob;

/// `aether.component.guest.prepare` — build a candidate from `code` beside
/// the running guest. The guest runs `unwire` and `on_dehydrate`, the
/// candidate instantiates with `config` (the guest's stored spawn config
/// when `None`) and rehydrates, and the running guest is kept until a
/// [`Commit`] or an [`Abort`]. Until then mail for the guest waits at its
/// inbox gate, and nothing the candidate sends leaves.
#[aether_data::kind(name = "aether.component.guest.prepare", no_serde)]
pub struct Prepare {
    /// The replacement module's code.
    pub code: Blob,
    /// The candidate's init config, or `None` to reuse the stored one.
    pub config: Option<Vec<u8>>,
}

/// `aether.component.guest.prepared` — the answer to [`Prepare`].
#[aether_data::kind(name = "aether.component.guest.prepared", no_serde)]
pub enum Prepared {
    /// The candidate is built and rehydrated, and waits for a commit or an
    /// abort.
    Ready,
    /// No candidate is held: the running guest is back in place, wired
    /// again if its hooks had run.
    Refused { error: String },
}

impl HeldReply for Prepared {
    fn unanswered() -> Self {
        Self::Refused { error: "closed".into() }
    }
}

/// `aether.component.guest.commit` — install the prepared candidate: its
/// held mail is sent on the commit's chain, and the mail its gate queued is
/// delivered to it in order.
#[aether_data::kind(name = "aether.component.guest.commit", default, no_serde)]
pub struct Commit;

/// `aether.component.guest.committed` — the answer to [`Commit`].
#[aether_data::kind(name = "aether.component.guest.committed", default, no_serde)]
pub struct Committed;

/// `aether.component.guest.abort` — discard the prepared candidate and its
/// held mail, and reinstate the running guest: its reply table and
/// correlation cursor come back, its `wire` runs again, and the mail its
/// gate queued is delivered to it in order. A guest with nothing prepared
/// answers at once.
#[aether_data::kind(name = "aether.component.guest.abort", default, no_serde)]
pub struct Abort;

/// `aether.component.guest.aborted` — the answer to [`Abort`].
#[aether_data::kind(name = "aether.component.guest.aborted", default, no_serde)]
pub struct Aborted;
