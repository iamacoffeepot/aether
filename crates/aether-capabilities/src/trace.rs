//! `aether.trace` cap (ADR-0080 §4). Receives [`BatchedTraceEvents`]
//! from the chassis-owned drainer thread and folds each event into
//! per-root counter maps + a parent → mail graph keyed by `MailId`.
//!
//! PR 2 of issue #707 ships only the state-tracking shape: the
//! observer accumulates `RootState` and `MailNode` entries, applies
//! retention + count-cap eviction, and exposes accessors for tests.
//! `Settled` mail emission is deferred to PR 3 alongside the chassis
//! sentinel + dispatcher switch that routes settlement replies into
//! the gate-site notification map.
//!
//! The observer's own dispatch of `BatchedTraceEvents` does not
//! generate further trace events: the drainer pushes its outbound
//! mail bare through `Mailer::push` (no `Sent` event), and the
//! producer hooks for `Received`/`Finished` short-circuit on
//! `MailId::NONE` (the default for mail not minted via
//! `NativeBinding::send_mail_with_lineage`). See ADR-0080 §7.

use aether_kinds::trace::BatchedTraceEvents;

#[aether_actor::bridge(singleton)]
mod native {
    use super::BatchedTraceEvents;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use aether_actor::actor;
    use aether_data::{KindId, MailId, MailboxId};
    use aether_kinds::trace::{Nanos, TraceEvent};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    /// ADR-0080 §11 retention defaults. Override via env vars.
    /// `AETHER_TRACE_RETENTION_MS` — drop roots older than this many
    /// milliseconds at end-of-handler. `AETHER_TRACE_MAX_ROOTS` —
    /// hard cap on root count; oldest evicted first when exceeded.
    /// Memory ceiling: ~50 MB at 100k roots × ~512 bytes/root
    /// (RootState + the typical handful of MailNodes per root).
    const RETENTION_MS_DEFAULT: u64 = 600_000;
    const MAX_ROOTS_DEFAULT: usize = 100_000;

    /// Per-root accumulator. `in_flight` tracks how many mails in
    /// this chain are currently between `Sent` and `Finished` —
    /// settlement is the moment this hits zero (PR 3 wires the
    /// `Settled` mail emission). PR 2 keeps the count for tests and
    /// future consumers.
    #[derive(Debug, Clone)]
    pub struct RootState {
        pub in_flight: u32,
        pub last_event_at: Instant,
    }

    /// Per-mail node in the parent → mail graph. `t_received` and
    /// `t_finished` patch as `Received`/`Finished` events arrive
    /// after the originating `Sent`.
    #[derive(Debug, Clone)]
    pub struct MailNode {
        pub root: MailId,
        pub parent: Option<MailId>,
        pub sender: MailboxId,
        pub recipient: MailboxId,
        pub kind: KindId,
        pub t_sent: Nanos,
        pub t_received: Option<Nanos>,
        pub t_finished: Option<Nanos>,
    }

    /// `aether.trace` mailbox cap. Folds [`BatchedTraceEvents`] into
    /// per-root counters and a parent → mail graph.
    pub struct TraceObserverCapability {
        roots: HashMap<MailId, RootState>,
        mails: HashMap<MailId, MailNode>,
        retention: Duration,
        max_roots: usize,
    }

    impl TraceObserverCapability {
        /// Read-only access to the per-root state map. Used by tests
        /// and (in PR 3) by `Settled` consumers; runtime callers
        /// should query via mail rather than reaching across threads.
        pub fn roots(&self) -> &HashMap<MailId, RootState> {
            &self.roots
        }

        /// Read-only access to the per-mail graph. Same access shape
        /// as [`Self::roots`].
        pub fn mails(&self) -> &HashMap<MailId, MailNode> {
            &self.mails
        }

        fn apply_event(&mut self, event: TraceEvent) {
            match event {
                TraceEvent::Sent {
                    mail_id,
                    root,
                    parent_mail,
                    sender,
                    recipient,
                    kind,
                    t,
                } => {
                    let now = Instant::now();
                    let root_state = self.roots.entry(root).or_insert(RootState {
                        in_flight: 0,
                        last_event_at: now,
                    });
                    root_state.in_flight = root_state.in_flight.saturating_add(1);
                    root_state.last_event_at = now;
                    self.mails.insert(
                        mail_id,
                        MailNode {
                            root,
                            parent: parent_mail,
                            sender,
                            recipient,
                            kind,
                            t_sent: t,
                            t_received: None,
                            t_finished: None,
                        },
                    );
                }
                TraceEvent::Received { mail_id, t } => {
                    if let Some(node) = self.mails.get_mut(&mail_id) {
                        node.t_received = Some(t);
                        if let Some(state) = self.roots.get_mut(&node.root) {
                            state.last_event_at = Instant::now();
                        }
                    }
                    // Orphan `Received` (no matching `Sent` ever
                    // observed) gets dropped. Eventual-consistency
                    // per ADR-0080 §6.
                }
                TraceEvent::Finished { mail_id, t } => {
                    if let Some(node) = self.mails.get_mut(&mail_id) {
                        node.t_finished = Some(t);
                        let root = node.root;
                        if let Some(state) = self.roots.get_mut(&root) {
                            state.in_flight = state.in_flight.saturating_sub(1);
                            state.last_event_at = Instant::now();
                            // PR 3 fires `Settled { root }` here when
                            // `state.in_flight` hits zero. PR 2 just
                            // observes the transition.
                        }
                    }
                }
            }
        }

        fn evict(&mut self) {
            let cutoff = Instant::now().checked_sub(self.retention);
            if let Some(cutoff) = cutoff {
                self.roots.retain(|_, state| state.last_event_at >= cutoff);
                self.mails
                    .retain(|_, node| self.roots.contains_key(&node.root));
            }
            // Hard cap: if we still exceed `max_roots`, drop oldest
            // by `last_event_at`. This is O(n) but only triggers on
            // overflow, so it amortises across the steady state.
            if self.roots.len() > self.max_roots {
                let mut entries: Vec<(MailId, Instant)> = self
                    .roots
                    .iter()
                    .map(|(id, state)| (*id, state.last_event_at))
                    .collect();
                entries.sort_by_key(|(_, t)| *t);
                let drop_n = self.roots.len() - self.max_roots;
                for (id, _) in entries.into_iter().take(drop_n) {
                    self.roots.remove(&id);
                }
                self.mails
                    .retain(|_, node| self.roots.contains_key(&node.root));
            }
        }
    }

    fn parse_env_u64(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }

    fn parse_env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }

    #[actor]
    impl NativeActor for TraceObserverCapability {
        type Config = ();
        // ADR-0080 §3 — `aether.trace` (matches
        // `aether_kinds::trace::TRACE_OBSERVER_MAILBOX_NAME`). Has to
        // be a literal here for the `#[actor]` macro's expansion.
        const NAMESPACE: &'static str = "aether.trace";

        fn init(_: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let retention_ms = parse_env_u64("AETHER_TRACE_RETENTION_MS", RETENTION_MS_DEFAULT);
            let max_roots = parse_env_usize("AETHER_TRACE_MAX_ROOTS", MAX_ROOTS_DEFAULT);
            Ok(Self {
                roots: HashMap::new(),
                mails: HashMap::new(),
                retention: Duration::from_millis(retention_ms),
                max_roots,
            })
        }

        /// ADR-0080 §4: fold every event in the batch into the
        /// per-root counter map and the parent → mail graph. Eviction
        /// runs once at end-of-handler so the per-event hot path is
        /// just a HashMap insert/update.
        ///
        /// # Agent
        /// Receives batched trace events from the chassis drainer
        /// thread. Each event is one producer-site emission (`Sent`
        /// at the sender, `Received` at the dispatcher entry,
        /// `Finished` at the dispatcher exit). PR 2 ships state
        /// only; PR 3 wires `Settled` reply emission for gate-site
        /// consumers (lifecycle, frame-loop drain).
        #[handler]
        fn on_batched_trace_events(&mut self, _ctx: &mut NativeCtx<'_>, batch: BatchedTraceEvents) {
            for event in batch.events {
                self.apply_event(event);
            }
            self.evict();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn boot_observer() -> TraceObserverCapability {
            // Use deterministic-friendly knobs so the eviction tests
            // don't have to wait on real time.
            unsafe {
                std::env::set_var("AETHER_TRACE_RETENTION_MS", "60000");
                std::env::set_var("AETHER_TRACE_MAX_ROOTS", "1000");
            }
            // Construct directly via init's logic (no chassis needed
            // for these state-fold tests).
            TraceObserverCapability {
                roots: HashMap::new(),
                mails: HashMap::new(),
                retention: Duration::from_millis(60_000),
                max_roots: 1000,
            }
        }

        fn mail(sender: u64, cid: u64) -> MailId {
            MailId {
                sender: MailboxId(sender),
                correlation_id: cid,
            }
        }

        #[test]
        fn sent_creates_root_and_node() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            obs.apply_event(TraceEvent::Sent {
                mail_id: m,
                root: m,
                parent_mail: None,
                sender: MailboxId(1),
                recipient: MailboxId(2),
                kind: KindId(0xABCD),
                t: Nanos(100),
            });
            assert_eq!(obs.roots.len(), 1);
            assert_eq!(obs.roots.get(&m).unwrap().in_flight, 1);
            assert_eq!(obs.mails.len(), 1);
            assert_eq!(obs.mails.get(&m).unwrap().t_sent, Nanos(100));
        }

        #[test]
        fn child_inherits_root_via_parent_mail() {
            let mut obs = boot_observer();
            let root = mail(1, 1);
            let child = mail(2, 1);
            obs.apply_event(TraceEvent::Sent {
                mail_id: root,
                root,
                parent_mail: None,
                sender: MailboxId(1),
                recipient: MailboxId(2),
                kind: KindId(0xABCD),
                t: Nanos(100),
            });
            obs.apply_event(TraceEvent::Sent {
                mail_id: child,
                root,
                parent_mail: Some(root),
                sender: MailboxId(2),
                recipient: MailboxId(3),
                kind: KindId(0xCDEF),
                t: Nanos(200),
            });
            assert_eq!(obs.roots.len(), 1);
            assert_eq!(obs.roots.get(&root).unwrap().in_flight, 2);
            assert_eq!(obs.mails.get(&child).unwrap().parent, Some(root));
        }

        #[test]
        fn finished_decrements_root_in_flight() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            obs.apply_event(TraceEvent::Sent {
                mail_id: m,
                root: m,
                parent_mail: None,
                sender: MailboxId(1),
                recipient: MailboxId(2),
                kind: KindId(0xABCD),
                t: Nanos(100),
            });
            obs.apply_event(TraceEvent::Received {
                mail_id: m,
                t: Nanos(200),
            });
            obs.apply_event(TraceEvent::Finished {
                mail_id: m,
                t: Nanos(300),
            });
            assert_eq!(obs.roots.get(&m).unwrap().in_flight, 0);
            let node = obs.mails.get(&m).unwrap();
            assert_eq!(node.t_received, Some(Nanos(200)));
            assert_eq!(node.t_finished, Some(Nanos(300)));
        }

        #[test]
        fn orphan_received_drops_silently() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            obs.apply_event(TraceEvent::Received {
                mail_id: m,
                t: Nanos(100),
            });
            assert!(obs.mails.is_empty());
            assert!(obs.roots.is_empty());
        }

        #[test]
        fn max_roots_evicts_oldest() {
            let mut obs = TraceObserverCapability {
                roots: HashMap::new(),
                mails: HashMap::new(),
                retention: Duration::from_secs(3600),
                max_roots: 3,
            };
            for cid in 1..=5 {
                let m = mail(1, cid);
                obs.apply_event(TraceEvent::Sent {
                    mail_id: m,
                    root: m,
                    parent_mail: None,
                    sender: MailboxId(1),
                    recipient: MailboxId(2),
                    kind: KindId(0xABCD),
                    t: Nanos(cid * 100),
                });
                // Tiny delay so `Instant::now()` advances across
                // each insert — cheap-enough for a 5-root test.
                std::thread::sleep(Duration::from_millis(2));
            }
            obs.evict();
            assert_eq!(obs.roots.len(), 3);
            // Oldest two (cid 1, 2) evicted; cid 3, 4, 5 retained.
            assert!(obs.roots.contains_key(&mail(1, 3)));
            assert!(obs.roots.contains_key(&mail(1, 4)));
            assert!(obs.roots.contains_key(&mail(1, 5)));
        }

        #[test]
        fn retention_evicts_stale() {
            let mut obs = TraceObserverCapability {
                roots: HashMap::new(),
                mails: HashMap::new(),
                retention: Duration::from_millis(50),
                max_roots: 1000,
            };
            let m = mail(1, 1);
            obs.apply_event(TraceEvent::Sent {
                mail_id: m,
                root: m,
                parent_mail: None,
                sender: MailboxId(1),
                recipient: MailboxId(2),
                kind: KindId(0xABCD),
                t: Nanos(100),
            });
            assert_eq!(obs.roots.len(), 1);
            std::thread::sleep(Duration::from_millis(80));
            obs.evict();
            assert!(obs.roots.is_empty());
            assert!(obs.mails.is_empty());
        }
    }
}
