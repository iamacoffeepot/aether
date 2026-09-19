//! One scripted reactor root: cursor, poison latch, and recording.
//!
//! [`Reactor`] stands in for a loaded reactor bundle root. It enforces the
//! sequencing the real root enforces — a `Warm` batch must start where the
//! cursor left off, and an `Event` must carry the next seq — answering
//! violations itself, so a core that mis-sequences fails loudly instead of
//! folding silently. Scripted replies come from the [`World`](super::World)
//! maps; the double only advances its cursor over replies it actually
//! returns, plus replies tests feed by hand through its [`note_evaluated`](Reactor::note_evaluated).

use aether_bloomery_kinds::{Detail, Evaluated, Warmed};

/// One scripted reactor root behind a mailbox.
#[derive(Debug, Default)]
pub struct Reactor {
    /// Last seq folded or evaluated, 0 when nothing is admitted yet.
    cursor: u64,
    /// Whether a fold or an evaluation poisoned this root.
    poisoned: bool,
    /// Every `Warm` range seen, in arrival order.
    pub warms: Vec<(u64, u64)>,
    /// Every `Event` seq seen, in arrival order.
    pub events: Vec<u64>,
}

impl Reactor {
    /// Answer one `Warm` batch, enforcing `first == cursor + 1`.
    ///
    /// A poisoned root answers poisoned and a mis-sequenced batch answers
    /// out-of-sequence, without consulting the scripted reply. Otherwise the
    /// scripted reply wins, then the default, then a fold through the batch.
    pub fn warm(&mut self, first: u64, last: u64, scripted: Option<Warmed>, default: Option<&Warmed>) -> Warmed {
        self.warms.push((first, last));
        if self.poisoned {
            return Warmed::Poisoned { last_trusted: self.cursor, reason: Detail::new("reactor is poisoned") };
        }
        if first != self.cursor + 1 {
            return Warmed::OutOfSequence { first, expected: self.cursor + 1 };
        }
        let reply = scripted.or_else(|| default.cloned()).unwrap_or(Warmed::Folded { through: last });
        self.note_warmed(&reply);
        reply
    }

    /// Check one `Event` ahead of its scripted reply.
    ///
    /// Returns `Some` when the double answers the violation itself: a
    /// poisoned root answers poisoned and a mis-sequenced event answers
    /// out-of-sequence. Returns `None` when the caller should answer from
    /// its scripted map or hold the event for the test to feed by hand.
    pub fn check_event(&mut self, seq: u64) -> Option<Evaluated> {
        self.events.push(seq);
        if self.poisoned {
            return Some(Evaluated::Poisoned {
                seq,
                last_trusted: self.cursor,
                reason: Detail::new("reactor is poisoned"),
            });
        }
        if seq != self.cursor + 1 {
            return Some(Evaluated::OutOfSequence { seq, expected: self.cursor + 1 });
        }
        None
    }

    /// Fold one returned `Evaluated` reply into the cursor and poison latch.
    ///
    /// Tests feeding a held event by hand call this alongside the core's
    /// `on_evaluated`, so later scripted events still sequence against the
    /// hand-fed reply.
    pub fn note_evaluated(&mut self, evaluated: &Evaluated) {
        match evaluated {
            Evaluated::Completed { seq, .. } | Evaluated::Failed { seq, .. } => {
                self.cursor = *seq;
            }
            Evaluated::Poisoned { .. } => {
                self.poisoned = true;
            }
            Evaluated::OutOfSequence { .. } => {}
        }
    }

    /// Fold one returned `Warmed` reply into the cursor and poison latch.
    fn note_warmed(&mut self, warmed: &Warmed) {
        match warmed {
            Warmed::Folded { through } => {
                self.cursor = *through;
            }
            Warmed::Poisoned { .. } => {
                self.poisoned = true;
            }
            Warmed::OutOfSequence { .. } => {}
        }
    }
}
