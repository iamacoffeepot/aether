//! Effect drain: verdict-then-effects, in recorded order, with echo
//! suppression (ADR-0137, issue 2687).
//!
//! After a filter call the host applies the verdict to the in-flight mail
//! first, then drains the effects in the order the script recorded them — so
//! stacked hosts see each other's forwards and never each other's in-flight
//! effects. The drain is expressed against a [`DrainSink`] so the ordering is
//! exercisable with a recording sink in a host-side test while the real host
//! routes each event to a relative cluster handle.
//!
//! **Echo suppression.** An effect the script sends down to its widget/child
//! provokes an up-lane echo (the widget re-emitting the value it just took).
//! The host records those kind/byte pairs in an [`EchoGuard`] at drain time
//! and, when the matching up-lane mail arrives, forwards it raw instead of
//! re-offering it to the same script — so a script's own write does not loop
//! back through its filter.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use aether_data::KindId;

use crate::envelope::{EffectTarget, FilterOutput, Verdict};

/// One ordered step of a drain, surfaced to a [`DrainSink`]. Owned so a
/// recording sink can assert both the sequence and the payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainEvent {
    /// The in-flight mail forwards along its lane, carrying `kind_id` and the
    /// verdict bytes (re-encoded when a `&mut K` handler mutated it).
    Forward {
        /// The forwarded kind's id (the inbound kind — a mutation does not
        /// change the kind).
        kind_id: u64,
        /// The forward payload.
        bytes: Vec<u8>,
    },
    /// One drained effect, delivered to `target`.
    Effect {
        /// Where the effect is delivered.
        target: EffectTarget,
        /// The effect kind's id.
        kind_id: u64,
        /// The effect kind's encoded payload.
        bytes: Vec<u8>,
    },
}

/// The sink a drain writes its ordered events into. The real host implements
/// it over the relative cluster handles; a test implements it over a `Vec`.
pub trait DrainSink {
    /// Record one drain event. Called verdict-first, then effects in order.
    fn record(&mut self, event: DrainEvent);
}

/// Drain `output`, applying the verdict first, then the effects in recorded
/// order. `forward_kind` is `Some(kind)` for a real in-flight mail (a
/// `Forward` verdict forwards the possibly-mutated bytes as that kind; a
/// `Consume` forwards nothing but still drains effects) and `None` for a
/// lifecycle-sentinel offer, which carries no in-flight mail — its verdict is
/// ignored and only its effects drain.
pub fn run_drain(output: FilterOutput, forward_kind: Option<KindId>, sink: &mut impl DrainSink) {
    if let (Some(kind), Verdict::Forward(bytes)) = (forward_kind, output.verdict) {
        sink.record(DrainEvent::Forward {
            kind_id: kind.0,
            bytes,
        });
    }
    for effect in output.effects {
        sink.record(DrainEvent::Effect {
            target: effect.target,
            kind_id: effect.kind_id,
            bytes: effect.bytes,
        });
    }
}

/// The effects a drain sends toward the wrapped widget or a named child — the
/// echoes to suppress when they return up-lane. A `Panel` effect goes up, so
/// it provokes no down-then-up echo and is not suppressed.
#[must_use]
pub fn echo_effects(output: &FilterOutput) -> Vec<(KindId, Vec<u8>)> {
    output
        .effects
        .iter()
        .filter(|e| matches!(e.target, EffectTarget::Widget | EffectTarget::Child(_)))
        .map(|e| (KindId(e.kind_id), e.bytes.clone()))
        .collect()
}

/// A one-shot multiset of up-lane echoes to suppress. Arming a kind/byte pair
/// after a down-lane effect means the next matching up-lane mail (the widget's
/// re-emit) is forwarded raw rather than re-offered to the script; the entry is
/// consumed as it fires so a later genuine value is offered normally.
#[derive(Default)]
pub struct EchoGuard {
    pending: BTreeMap<(KindId, Vec<u8>), u32>,
}

impl EchoGuard {
    /// Arm one expected up-lane echo per `(kind, bytes)` pair in `effects`.
    pub fn arm(&mut self, effects: impl IntoIterator<Item = (KindId, Vec<u8>)>) {
        for effect in effects {
            *self.pending.entry(effect).or_insert(0) += 1;
        }
    }

    /// If an echo of `(kind, bytes)` is pending, consume one and return `true`
    /// (the caller forwards raw, skipping the filter). Otherwise `false`.
    pub fn take(&mut self, kind: KindId, bytes: &[u8]) -> bool {
        let key = (kind, bytes.to_vec());
        match self.pending.get_mut(&key) {
            Some(count) => {
                *count -= 1;
                if *count == 0 {
                    self.pending.remove(&key);
                }
                true
            }
            None => false,
        }
    }

    /// Whether any echoes are currently armed (test observability).
    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Effect;
    use alloc::vec;

    #[derive(Default)]
    struct RecordingSink {
        events: Vec<DrainEvent>,
    }
    impl DrainSink for RecordingSink {
        fn record(&mut self, event: DrainEvent) {
            self.events.push(event);
        }
    }

    fn effect(target: EffectTarget, kind_id: u64) -> Effect {
        Effect {
            target,
            kind_id,
            bytes: vec![kind_id as u8],
        }
    }

    // Tripwire: the in-flight forward is recorded before any effect — the
    // fixed verdict-then-effects order stacked hosts rely on. An out-of-order
    // drain (effects before forward) would let a stacked host observe an
    // in-flight effect.
    #[test]
    fn forward_precedes_effects_in_order() {
        let output = FilterOutput {
            verdict: Verdict::Forward(vec![9]),
            effects: vec![
                effect(EffectTarget::Widget, 1),
                effect(EffectTarget::Panel, 2),
            ],
        };
        let mut sink = RecordingSink::default();
        run_drain(output, Some(KindId(0x55)), &mut sink);
        assert_eq!(
            sink.events,
            vec![
                DrainEvent::Forward {
                    kind_id: 0x55,
                    bytes: vec![9]
                },
                DrainEvent::Effect {
                    target: EffectTarget::Widget,
                    kind_id: 1,
                    bytes: vec![1]
                },
                DrainEvent::Effect {
                    target: EffectTarget::Panel,
                    kind_id: 2,
                    bytes: vec![2]
                },
            ]
        );
    }

    // Tripwire: a consumed verdict emits no forward but still drains its
    // effects — consume-plus-emit substitutes for a forward.
    #[test]
    fn consume_drains_effects_without_forward() {
        let output = FilterOutput {
            verdict: Verdict::Consume,
            effects: vec![effect(EffectTarget::Child("slider".into()), 7)],
        };
        let mut sink = RecordingSink::default();
        run_drain(output, Some(KindId(0x55)), &mut sink);
        assert_eq!(
            sink.events,
            vec![DrainEvent::Effect {
                target: EffectTarget::Child("slider".into()),
                kind_id: 7,
                bytes: vec![7],
            }]
        );
    }

    // Tripwire: a sentinel offer (`forward_kind = None`) drains its effects
    // but never forwards its verdict — a lifecycle hook carries no in-flight
    // mail, so a stray `Forward(empty)` must not leak out as real mail.
    #[test]
    fn sentinel_offer_drains_effects_without_forward() {
        let output = FilterOutput {
            verdict: Verdict::Forward(Vec::new()),
            effects: vec![effect(EffectTarget::Widget, 5)],
        };
        let mut sink = RecordingSink::default();
        run_drain(output, None, &mut sink);
        assert_eq!(
            sink.events,
            vec![DrainEvent::Effect {
                target: EffectTarget::Widget,
                kind_id: 5,
                bytes: vec![5]
            }]
        );
    }

    // Tripwire: a script whose effect targets its own widget/child arms that
    // exact kind/bytes pair for suppression, and the one-shot guard skips only
    // the matching up-lane echo — so a real same-kind value that arrives first
    // still reaches the filter. A Panel effect is not suppressed (it goes up,
    // no echo).
    #[test]
    fn echo_of_own_write_is_value_correlated() {
        let output = FilterOutput {
            verdict: Verdict::Forward(vec![0]),
            effects: vec![
                Effect {
                    target: EffectTarget::Widget,
                    kind_id: 42,
                    bytes: vec![1, 2, 3],
                },
                effect(EffectTarget::Child("slider".into()), 7),
                Effect {
                    target: EffectTarget::Widget,
                    kind_id: 42,
                    bytes: vec![1, 2, 3],
                },
                effect(EffectTarget::Panel, 99),
            ],
        };
        let mut guard = EchoGuard::default();
        guard.arm(echo_effects(&output));

        // The panel effect provoked no armed echo.
        assert!(!guard.take(KindId(99), &[99]));
        // Child-targeted effects arm an echo just like widget-targeted ones.
        assert!(guard.take(KindId(7), &[7]));
        assert!(!guard.take(KindId(7), &[7]));
        // A genuine same-kind value with different bytes is not consumed.
        assert!(!guard.take(KindId(42), &[7, 8, 9]));
        // The exact own-write echo is suppressed twice, then no longer armed.
        assert!(guard.take(KindId(42), &[1, 2, 3]));
        assert!(guard.take(KindId(42), &[1, 2, 3]));
        assert!(!guard.take(KindId(42), &[1, 2, 3]));
        assert!(guard.is_empty());
    }
}
