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

use aether_kinds::trace::{
    BatchedTraceEvents, DescribeTree, DescribeTreeResult, DescribeWindow, DescribeWindowResult,
    ListActiveRoots, ListActiveRootsResult, MailNodeWire, RootSummaryWire, TraceWindow,
};

#[aether_actor::bridge(singleton)]
mod native {
    use super::{
        BatchedTraceEvents, DescribeTree, DescribeTreeResult, DescribeWindow, DescribeWindowResult,
        ListActiveRoots, ListActiveRootsResult, MailNodeWire, RootSummaryWire, TraceWindow,
    };
    use std::collections::{BTreeSet, HashMap};
    use std::ops::Bound;
    use std::time::{Duration, Instant};

    use std::sync::Arc;

    use aether_actor::{MailCtx, actor};
    use aether_data::{KindId, MailId, MailboxId};
    use aether_kinds::trace::{Nanos, Settled, TraceEvent};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::Mail;
    use aether_substrate::mail::mailer::Mailer;

    /// ADR-0080 §11 retention defaults. Override via env vars.
    /// `AETHER_TRACE_RETENTION_MS` — drop roots older than this many
    /// milliseconds at end-of-handler. `AETHER_TRACE_MAX_ROOTS` —
    /// hard cap on root count; oldest evicted first when exceeded.
    /// Memory ceiling: ~50 MB at 100k roots × ~512 bytes/root
    /// (`RootState` + the typical handful of `MailNodes` per root).
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
    /// after the originating `Sent`. Issue 734: `thread_name` patches
    /// from the `Received` event the same way `t_received` does — the
    /// dispatcher captures `std::thread::current().name()` so the
    /// trace renderer (`hub::mcp::trace`) can give each actor its
    /// own per-thread row.
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
        pub thread_name: Option<String>,
    }

    /// `aether.trace` mailbox cap. Folds [`BatchedTraceEvents`] into
    /// per-root counters and a parent → mail graph; emits `Settled`
    /// to [`MailboxId::CHASSIS_MAILBOX_ID`] when a root's `in_flight`
    /// count transitions to zero (ADR-0080 §6).
    pub struct TraceObserverCapability {
        roots: HashMap<MailId, RootState>,
        mails: HashMap<MailId, MailNode>,
        /// Issue 735: secondary index by `t_sent` for the time-window
        /// query path (`describe_window`). `BTreeSet<(Nanos, MailId)>`
        /// because two mails *could* share a `Nanos` (synthetic test
        /// timestamps; in production `Nanos` is monotonic ~10ns
        /// granularity, but tight back-to-back sends could collide).
        /// Inserted on `TraceEvent::Sent`, removed in `evict` whenever
        /// a `MailNode` drops.
        t_sent_index: BTreeSet<(Nanos, MailId)>,
        retention: Duration,
        max_roots: usize,
        /// Mailer handle stashed at init so `Settled` mail can be
        /// pushed bare via [`Mailer::push`] — bypassing
        /// `NativeBinding::send_mail_with_lineage` so the outbound
        /// doesn't generate a `TraceEvent::Sent` (which would mint
        /// a fresh `mail_id` chain whose `Finished` never fires —
        /// chassis-router-routed mail doesn't trip the dispatcher's
        /// Received/Finished hooks).
        mailer: Arc<Mailer>,
        /// Cached `Settled` kind id; computed once at init to avoid
        /// re-resolving for every settlement event.
        settled_kind: KindId,
    }

    impl TraceObserverCapability {
        /// Read-only access to the per-root state map. Used by tests
        /// and (in PR 3) by `Settled` consumers; runtime callers
        /// should query via mail rather than reaching across threads.
        #[must_use]
        pub fn roots(&self) -> &HashMap<MailId, RootState> {
            &self.roots
        }

        /// Read-only access to the per-mail graph. Same access shape
        /// as [`Self::roots`].
        #[must_use]
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
                            thread_name: None,
                        },
                    );
                    self.t_sent_index.insert((t, mail_id));
                }
                TraceEvent::Received {
                    mail_id,
                    t,
                    thread_name,
                } => {
                    if let Some(node) = self.mails.get_mut(&mail_id) {
                        node.t_received = Some(t);
                        node.thread_name = thread_name;
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
                            // ADR-0080 §6: settlement fires when the
                            // chain's in-flight count transitions to
                            // zero. We push `Settled { root }` to
                            // `CHASSIS_MAILBOX_ID` via the bare mailer
                            // so the outbound doesn't generate trace
                            // events; the chassis-router decodes the
                            // payload and signals every gate-site
                            // subscriber waiting on this root.
                            if state.in_flight == 0 {
                                self.fire_settled(root);
                            }
                        }
                    }
                }
            }
        }

        /// Issue 718: pure compute path for `on_describe_tree` —
        /// extracted so tests can exercise filtering without a
        /// `NativeCtx` (the handler is a thin reply wrapper).
        pub(crate) fn build_describe_tree(&self, root: MailId) -> DescribeTreeResult {
            let Some(root_state) = self.roots.get(&root) else {
                return DescribeTreeResult::Err { not_found: root };
            };
            let mails: Vec<MailNodeWire> = self
                .mails
                .iter()
                .filter(|(_, node)| node.root == root)
                .map(|(mail_id, node)| mail_node_wire_from(*mail_id, node))
                .collect();
            DescribeTreeResult::Ok {
                root,
                in_flight: root_state.in_flight,
                mails,
            }
        }

        /// Issue 718: pure compute path for `on_list_active_roots`.
        /// `now` is injected so tests can drive deterministic windows
        /// without depending on `SUBSTRATE_START` being initialised.
        pub(crate) fn build_list_active_roots(
            &self,
            request: ListActiveRoots,
            now: Nanos,
        ) -> ListActiveRootsResult {
            const DEFAULT_SINCE_MS: u32 = 60_000;
            const DEFAULT_MAX: u32 = 50;
            const HARD_MAX: u32 = 1000;

            let since_ms = request.since_ms.unwrap_or(DEFAULT_SINCE_MS);
            let max = request.max.unwrap_or(DEFAULT_MAX).min(HARD_MAX) as usize;
            let cutoff_ns = u64::from(since_ms).saturating_mul(1_000_000);

            let mut summaries: Vec<RootSummaryWire> = self
                .roots
                .iter()
                .filter_map(|(root_id, root_state)| {
                    let node = self.mails.get(root_id)?;
                    if now.0.saturating_sub(node.t_sent.0) > cutoff_ns {
                        return None;
                    }
                    Some(RootSummaryWire {
                        root: *root_id,
                        kind: node.kind,
                        sender: node.sender,
                        recipient: node.recipient,
                        t_sent: node.t_sent,
                        in_flight: root_state.in_flight,
                    })
                })
                .collect();
            summaries.sort_by_key(|s| std::cmp::Reverse(s.t_sent));
            summaries.truncate(max);
            ListActiveRootsResult { roots: summaries }
        }

        /// Issue 735: pure compute path for `on_describe_window`.
        /// `now` is injected so tests can drive deterministic windows
        /// without depending on `SUBSTRATE_START` being initialised.
        ///
        /// Strict `t_sent` containment: a mail belongs to the window
        /// iff `start_ns <= t_sent <= end_ns`. Counts the matched set
        /// before allocating the reply so an over-cap query returns
        /// `Err { too_many: Some(matched) }` instead of a partial
        /// vector — see issue body for the design rationale.
        pub(crate) fn build_describe_window(
            &self,
            request: DescribeWindow,
            now: Nanos,
        ) -> DescribeWindowResult {
            const DEFAULT_MAX: u32 = 10_000;
            const HARD_MAX: u32 = 100_000;

            let max = request.max_mails.unwrap_or(DEFAULT_MAX).min(HARD_MAX) as usize;

            let (start, end) = match request.window {
                TraceWindow::Absolute { start_ns, end_ns } => {
                    (Nanos(start_ns), Nanos(end_ns.unwrap_or(u64::MAX)))
                }
                TraceWindow::Relative { last_ms } => {
                    let last_ns = last_ms.saturating_mul(1_000_000);
                    (Nanos(now.0.saturating_sub(last_ns)), now)
                }
            };

            // BTreeSet range: lower bound is `(start, MailId::NONE)`;
            // upper bound is `(end + 1ns, MailId::NONE)` excluded so
            // the inclusive end timestamp is captured for every
            // `MailId` tied to it. `saturating_add` covers the (vanishing)
            // edge where `end == u64::MAX`.
            let upper = Nanos(end.0.saturating_add(1));
            let range = self.t_sent_index.range((
                Bound::Included((start, MailId::NONE)),
                Bound::Excluded((upper, MailId::NONE)),
            ));

            // Two-pass: count first to honour the cap-or-error contract,
            // then collect. `BTreeSet::range` is cheap to iterate twice
            // (O(log N + matches) per pass).
            let count = range.clone().count();
            if count > max {
                // `max` is u32 on the wire; `count` is bounded by the
                // BTreeSet which can't exceed `u32::MAX` in any realistic
                // tracing window.
                #[allow(clippy::cast_possible_truncation)]
                return DescribeWindowResult::Err {
                    too_many: Some(count as u32),
                };
            }

            let mails: Vec<MailNodeWire> = range
                .filter_map(|(_, mid)| {
                    let node = self.mails.get(mid)?;
                    Some(mail_node_wire_from(*mid, node))
                })
                .collect();

            DescribeWindowResult::Ok { mails }
        }

        fn fire_settled(&self, root: MailId) {
            let payload = match postcard::to_allocvec(&Settled { root }) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(
                        target: "aether_capabilities::trace",
                        root = ?root,
                        error = %e,
                        "Settled encode failed; chassis subscribers not notified",
                    );
                    return;
                }
            };
            self.mailer.push(Mail::new(
                MailboxId::CHASSIS_MAILBOX_ID,
                self.settled_kind,
                payload,
                1,
            ));
        }

        fn evict(&mut self) {
            let cutoff = Instant::now().checked_sub(self.retention);
            if let Some(cutoff) = cutoff {
                self.roots.retain(|_, state| state.last_event_at >= cutoff);
                self.drop_orphaned_mails();
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
                self.drop_orphaned_mails();
            }
        }

        /// Drops every `MailNode` whose root is no longer in
        /// `self.roots`, and removes the matching entry from
        /// `self.t_sent_index`. Issue 735: keeping the secondary
        /// index in sync is the load-bearing part — a stale entry
        /// would cause `describe_window` to surface a `MailId` that
        /// `self.mails.get` can't resolve (handled by `filter_map`,
        /// but it's a silent miscount of the matched set).
        fn drop_orphaned_mails(&mut self) {
            let dropped: Vec<MailId> = self
                .mails
                .iter()
                .filter(|(_, node)| !self.roots.contains_key(&node.root))
                .map(|(mid, _)| *mid)
                .collect();
            for mid in &dropped {
                if let Some(node) = self.mails.remove(mid) {
                    self.t_sent_index.remove(&(node.t_sent, *mid));
                }
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

    /// Builds the wire-shaped projection of a `MailNode` for the
    /// `describe_tree` / `describe_window` replies. Consolidates the
    /// two near-identical struct-literal sites that differ only in
    /// how the caller obtains the `(mail_id, node)` pair.
    fn mail_node_wire_from(mail_id: MailId, node: &MailNode) -> MailNodeWire {
        MailNodeWire {
            mail_id,
            parent: node.parent,
            sender: node.sender,
            recipient: node.recipient,
            kind: node.kind,
            t_sent: node.t_sent,
            t_received: node.t_received,
            t_finished: node.t_finished,
            thread_name: node.thread_name.clone(),
        }
    }

    #[actor]
    impl NativeActor for TraceObserverCapability {
        type Config = ();
        // ADR-0080 §3 — `aether.trace` (matches
        // `aether_kinds::trace::TRACE_OBSERVER_MAILBOX_NAME`). Has to
        // be a literal here for the `#[actor]` macro's expansion.
        const NAMESPACE: &'static str = "aether.trace";

        fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let retention_ms = parse_env_u64("AETHER_TRACE_RETENTION_MS", RETENTION_MS_DEFAULT);
            let max_roots = parse_env_usize("AETHER_TRACE_MAX_ROOTS", MAX_ROOTS_DEFAULT);
            Ok(Self {
                roots: HashMap::new(),
                mails: HashMap::new(),
                t_sent_index: BTreeSet::new(),
                retention: Duration::from_millis(retention_ms),
                max_roots,
                mailer: ctx.mailer(),
                settled_kind: <Settled as aether_data::Kind>::ID,
            })
        }

        /// ADR-0080 §4: fold every event in the batch into the
        /// per-root counter map and the parent → mail graph. Eviction
        /// runs once at end-of-handler so the per-event hot path is
        /// just a `HashMap` insert/update.
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

        /// # Agent
        /// Returns the mail tree for one root: every node currently in
        /// the observer's `mails` map whose `root` matches the request,
        /// plus the root's current `in_flight` count. Replies
        /// `Err::not_found` when the root isn't tracked (never seen or
        /// evicted past retention). Issue 718 / ADR-0080 Phase 2.
        #[handler]
        fn on_describe_tree(&mut self, ctx: &mut NativeCtx<'_>, request: DescribeTree) {
            let result = self.build_describe_tree(request.root);
            ctx.reply(&result);
        }

        /// # Agent
        /// Returns recent root summaries for agent root-discovery.
        /// `since_ms` filters by the root's originating `Sent`
        /// timestamp (default `60_000`); `max` caps the reply length
        /// (default 50, hard cap 1000). Sorted by `t_sent` descending.
        /// Issue 718 / ADR-0080 Phase 2.
        #[handler]
        fn on_list_active_roots(&mut self, ctx: &mut NativeCtx<'_>, request: ListActiveRoots) {
            let now = aether_substrate::runtime::trace::now_nanos();
            let result = self.build_list_active_roots(request, now);
            ctx.reply(&result);
        }

        /// # Agent
        /// Returns every mail in the observer whose `t_sent` falls
        /// within the requested window (strict containment). Window
        /// can be absolute nanoseconds or `Relative { last_ms }`
        /// (resolved against the substrate's monotonic now).
        /// `max_mails` caps the reply size (default `10_000`, hard cap
        /// `100_000`); over-cap windows reply `Err { too_many: Some(n)
        /// }` so the caller can narrow the window. Parent edges may
        /// dangle to mail outside the window — drill into a specific
        /// root via `describe_tree` for full chain context. Issue 735.
        #[handler]
        fn on_describe_window(&mut self, ctx: &mut NativeCtx<'_>, request: DescribeWindow) {
            let now = aether_substrate::runtime::trace::now_nanos();
            let result = self.build_describe_window(request, now);
            ctx.reply(&result);
        }
    }

    #[cfg(test)]
    // Tests hold the capture `Mutex` guard across the assertion block
    // so the snapshot reads atomically against the concurrent
    // observer-side push.
    #[allow(clippy::significant_drop_tightening)]
    mod tests {
        use super::*;
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::registry::Registry;

        /// Construct an observer for state-fold tests. Stash a fresh
        /// `Mailer` so `fire_settled` has somewhere to push (the
        /// chassis-router isn't installed, so the bare push warn-drops
        /// at the `route_mail` switch — that's fine for state assertions).
        fn boot_observer() -> TraceObserverCapability {
            // SAFETY: called only from this test thread before any
            // reader. `AETHER_TRACE_RETENTION_MS` and
            // `AETHER_TRACE_MAX_ROOTS` are read inside `observer_with`
            // (the next call on this same thread) and nowhere else in
            // this test module, so no concurrent reader can race the
            // write. `std::env::set_var` only requires "no other thread
            // is reading or writing the environment simultaneously."
            unsafe {
                std::env::set_var("AETHER_TRACE_RETENTION_MS", "60000");
                std::env::set_var("AETHER_TRACE_MAX_ROOTS", "1000");
            }
            observer_with(Duration::from_millis(60_000), 1000)
        }

        fn observer_with(retention: Duration, max_roots: usize) -> TraceObserverCapability {
            let registry = Arc::new(Registry::new());
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let mailer = Arc::new(Mailer::new(registry, store));
            TraceObserverCapability {
                roots: HashMap::new(),
                mails: HashMap::new(),
                t_sent_index: BTreeSet::new(),
                retention,
                max_roots,
                mailer,
                settled_kind: <Settled as aether_data::Kind>::ID,
            }
        }

        fn mail(sender: u64, cid: u64) -> MailId {
            MailId {
                sender: MailboxId(sender),
                correlation_id: cid,
            }
        }

        /// Consolidates the `obs.apply_event(TraceEvent::Sent { ... })`
        /// call-site that recurs across every state-fold test. Same
        /// field order as the variant so call sites read positionally.
        #[allow(clippy::too_many_arguments)]
        fn apply_sent_event(
            obs: &mut TraceObserverCapability,
            mail_id: MailId,
            root: MailId,
            parent_mail: Option<MailId>,
            sender: MailboxId,
            recipient: MailboxId,
            kind: KindId,
            t: Nanos,
        ) {
            obs.apply_event(TraceEvent::Sent {
                mail_id,
                root,
                parent_mail,
                sender,
                recipient,
                kind,
                t,
            });
        }

        #[test]
        fn sent_creates_root_and_node() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            apply_sent_event(
                &mut obs,
                m,
                m,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            assert_eq!(obs.roots.len(), 1);
            assert_eq!(
                obs.roots
                    .get(&m)
                    .expect("root entry exists for mail")
                    .in_flight,
                1
            );
            assert_eq!(obs.mails.len(), 1);
            assert_eq!(
                obs.mails.get(&m).expect("mail node exists for mail").t_sent,
                Nanos(100)
            );
        }

        #[test]
        fn child_inherits_root_via_parent_mail() {
            let mut obs = boot_observer();
            let root = mail(1, 1);
            let child = mail(2, 1);
            apply_sent_event(
                &mut obs,
                root,
                root,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            apply_sent_event(
                &mut obs,
                child,
                root,
                Some(root),
                MailboxId(2),
                MailboxId(3),
                KindId(0xCDEF),
                Nanos(200),
            );
            assert_eq!(obs.roots.len(), 1);
            assert_eq!(
                obs.roots
                    .get(&root)
                    .expect("root entry exists for root mail")
                    .in_flight,
                2
            );
            assert_eq!(
                obs.mails
                    .get(&child)
                    .expect("child mail node exists")
                    .parent,
                Some(root)
            );
        }

        #[test]
        fn finished_decrements_root_in_flight() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            apply_sent_event(
                &mut obs,
                m,
                m,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            obs.apply_event(TraceEvent::Received {
                mail_id: m,
                t: Nanos(200),
                thread_name: Some("aether-root-test".to_owned()),
            });
            obs.apply_event(TraceEvent::Finished {
                mail_id: m,
                t: Nanos(300),
            });
            assert_eq!(
                obs.roots
                    .get(&m)
                    .expect("root entry exists for mail")
                    .in_flight,
                0
            );
            let node = obs.mails.get(&m).expect("mail node exists for mail");
            assert_eq!(node.t_received, Some(Nanos(200)));
            assert_eq!(node.t_finished, Some(Nanos(300)));
            assert_eq!(node.thread_name.as_deref(), Some("aether-root-test"));
        }

        #[test]
        fn orphan_received_drops_silently() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            obs.apply_event(TraceEvent::Received {
                mail_id: m,
                t: Nanos(100),
                thread_name: None,
            });
            assert!(obs.mails.is_empty());
            assert!(obs.roots.is_empty());
        }

        #[test]
        fn max_roots_evicts_oldest() {
            let mut obs = observer_with(Duration::from_secs(3600), 3);
            for cid in 1..=5 {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(cid * 100),
                );
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
        fn finished_to_zero_fires_settled() {
            use std::sync::Mutex;

            // Build a Mailer with a chassis-router that records every
            // chassis-addressed mail. The observer's `fire_settled`
            // pushes through the Mailer; the router intercepts.
            let registry = Arc::new(Registry::new());
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let mailer = Arc::new(Mailer::new(registry, store));
            let captured: Arc<Mutex<Vec<Settled>>> = Arc::new(Mutex::new(Vec::new()));
            let captured_for_router = Arc::clone(&captured);
            let settled_kind = <Settled as aether_data::Kind>::ID;
            mailer.install_chassis_router(Box::new(move |mail| {
                if mail.kind == settled_kind
                    && let Ok(notice) = postcard::from_bytes::<Settled>(&mail.payload)
                {
                    captured_for_router
                        .lock()
                        .expect("test stub: captured mutex poisoned")
                        .push(notice);
                }
            }));

            let mut obs = TraceObserverCapability {
                roots: HashMap::new(),
                mails: HashMap::new(),
                t_sent_index: BTreeSet::new(),
                retention: Duration::from_secs(60),
                max_roots: 1000,
                mailer: Arc::clone(&mailer),
                settled_kind,
            };

            let root = mail(1, 1);
            apply_sent_event(
                &mut obs,
                root,
                root,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            // No Settled yet — in_flight is 1.
            assert!(
                captured
                    .lock()
                    .expect("test stub: captured mutex poisoned")
                    .is_empty()
            );
            obs.apply_event(TraceEvent::Finished {
                mail_id: root,
                t: Nanos(200),
            });
            // Settled fired; chassis-router decoded the mail.
            let captured = captured.lock().expect("test stub: captured mutex poisoned");
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].root, root);
        }

        #[test]
        fn describe_tree_returns_full_subtree() {
            let mut obs = boot_observer();
            let root = mail(1, 1);
            let a = mail(2, 1);
            let b = mail(2, 2);
            let unrelated = mail(9, 9);
            apply_sent_event(
                &mut obs,
                root,
                root,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            apply_sent_event(
                &mut obs,
                a,
                root,
                Some(root),
                MailboxId(2),
                MailboxId(3),
                KindId(0xCDEF),
                Nanos(200),
            );
            apply_sent_event(
                &mut obs,
                b,
                root,
                Some(root),
                MailboxId(2),
                MailboxId(4),
                KindId(0xDEAD),
                Nanos(300),
            );
            apply_sent_event(
                &mut obs,
                unrelated,
                unrelated,
                None,
                MailboxId(9),
                MailboxId(8),
                KindId(0xBEEF),
                Nanos(400),
            );

            let result = obs.build_describe_tree(root);
            match result {
                DescribeTreeResult::Ok {
                    root: r,
                    in_flight,
                    mails,
                } => {
                    assert_eq!(r, root);
                    assert_eq!(in_flight, 3);
                    assert_eq!(mails.len(), 3);
                    let ids: std::collections::HashSet<MailId> =
                        mails.iter().map(|m| m.mail_id).collect();
                    assert!(ids.contains(&root));
                    assert!(ids.contains(&a));
                    assert!(ids.contains(&b));
                    assert!(!ids.contains(&unrelated));
                }
                DescribeTreeResult::Err { not_found } => {
                    panic!("expected Ok, got Err::not_found {not_found:?}")
                }
            }
        }

        #[test]
        fn describe_tree_unknown_root_returns_err() {
            let obs = boot_observer();
            let missing = mail(7, 7);
            assert_eq!(
                obs.build_describe_tree(missing),
                DescribeTreeResult::Err { not_found: missing }
            );
        }

        #[test]
        fn list_active_roots_filters_by_window_and_sorts() {
            let mut obs = boot_observer();
            // Three roots at t = 100, 5_000_000_000 (5s), 10_000_000_000 (10s).
            // Window since_ms = 6000 keeps the latter two.
            for (cid, t) in [(1u64, 100u64), (2, 5_000_000_000), (3, 10_000_000_000)] {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(t),
                );
            }

            // "Now" is 11s past boot.
            let now = Nanos(11_000_000_000);
            let result = obs.build_list_active_roots(
                ListActiveRoots {
                    since_ms: Some(6_000),
                    max: None,
                },
                now,
            );
            assert_eq!(result.roots.len(), 2);
            // Sorted desc by t_sent — newer first.
            assert_eq!(result.roots[0].root, mail(1, 3));
            assert_eq!(result.roots[1].root, mail(1, 2));
        }

        #[test]
        fn list_active_roots_caps_to_max() {
            let mut obs = boot_observer();
            for cid in 1..=5 {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(cid * 100),
                );
            }
            let result = obs.build_list_active_roots(
                ListActiveRoots {
                    since_ms: Some(60_000),
                    max: Some(2),
                },
                Nanos(1_000),
            );
            assert_eq!(result.roots.len(), 2);
            // Top 2 by t_sent desc: cid 5 (t=500), cid 4 (t=400).
            assert_eq!(result.roots[0].root, mail(1, 5));
            assert_eq!(result.roots[1].root, mail(1, 4));
        }

        #[test]
        fn t_sent_index_inserts_on_sent() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            apply_sent_event(
                &mut obs,
                m,
                m,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(500),
            );
            assert!(obs.t_sent_index.contains(&(Nanos(500), m)));
        }

        #[test]
        fn t_sent_index_drops_on_evict() {
            let mut obs = observer_with(Duration::from_millis(50), 1000);
            let m = mail(1, 1);
            apply_sent_event(
                &mut obs,
                m,
                m,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            assert_eq!(obs.t_sent_index.len(), 1);
            std::thread::sleep(Duration::from_millis(80));
            obs.evict();
            assert!(obs.t_sent_index.is_empty(), "secondary index out of sync");
        }

        #[test]
        fn t_sent_index_drops_on_max_roots_overflow() {
            // Exceeding `max_roots` evicts the oldest by `last_event_at`,
            // and `drop_orphaned_mails` should prune the secondary
            // index for the evicted mails too.
            let mut obs = observer_with(Duration::from_secs(3600), 3);
            for cid in 1..=5u64 {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(cid * 100),
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            obs.evict();
            assert_eq!(obs.roots.len(), 3);
            assert_eq!(
                obs.t_sent_index.len(),
                3,
                "secondary index out of sync with mails"
            );
            // Index entries should match the surviving mails (cid 3, 4, 5).
            assert!(obs.t_sent_index.contains(&(Nanos(300), mail(1, 3))));
            assert!(obs.t_sent_index.contains(&(Nanos(400), mail(1, 4))));
            assert!(obs.t_sent_index.contains(&(Nanos(500), mail(1, 5))));
        }

        #[test]
        fn describe_window_returns_in_window_mails() {
            let mut obs = boot_observer();
            // Three sends at t = 100, 500, 900.
            for (cid, t) in [(1u64, 100u64), (2, 500), (3, 900)] {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(t),
                );
            }
            // Window [200, 800] strictly contains only the t=500 mail.
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Absolute {
                        start_ns: 200,
                        end_ns: Some(800),
                    },
                    max_mails: None,
                },
                Nanos(1_000),
            );
            match result {
                DescribeWindowResult::Ok { mails } => {
                    assert_eq!(mails.len(), 1);
                    assert_eq!(mails[0].mail_id, mail(1, 2));
                }
                DescribeWindowResult::Err { too_many } => {
                    panic!("expected Ok, got Err::too_many {too_many:?}")
                }
            }
        }

        #[test]
        fn describe_window_inclusive_at_boundaries() {
            let mut obs = boot_observer();
            for (cid, t) in [(1u64, 200u64), (2, 800)] {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(t),
                );
            }
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Absolute {
                        start_ns: 200,
                        end_ns: Some(800),
                    },
                    max_mails: None,
                },
                Nanos(1_000),
            );
            match result {
                DescribeWindowResult::Ok { mails } => assert_eq!(mails.len(), 2),
                DescribeWindowResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn describe_window_collisions_at_same_t_sent() {
            // BTreeSet<(Nanos, MailId)>: two mails at the same Nanos
            // tie-break by MailId and both survive in the index.
            let mut obs = boot_observer();
            for cid in 1..=3u64 {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(500),
                );
            }
            assert_eq!(obs.t_sent_index.len(), 3);
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Absolute {
                        start_ns: 500,
                        end_ns: Some(500),
                    },
                    max_mails: None,
                },
                Nanos(1_000),
            );
            match result {
                DescribeWindowResult::Ok { mails } => assert_eq!(mails.len(), 3),
                DescribeWindowResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn describe_window_too_many_returns_err() {
            let mut obs = boot_observer();
            for cid in 1..=10u64 {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(cid * 10),
                );
            }
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Absolute {
                        start_ns: 0,
                        end_ns: Some(1_000),
                    },
                    max_mails: Some(5),
                },
                Nanos(1_000),
            );
            assert_eq!(result, DescribeWindowResult::Err { too_many: Some(10) });
        }

        #[test]
        fn describe_window_relative_resolves_against_now() {
            let mut obs = boot_observer();
            // Sends at t = 1s, 5s, 10s (in nanos).
            for (cid, t) in [
                (1u64, 1_000_000_000u64),
                (2, 5_000_000_000),
                (3, 10_000_000_000),
            ] {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(t),
                );
            }
            // now = 11s, last_ms = 6_000 → window [5s, 11s] keeps cids 2, 3.
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Relative { last_ms: 6_000 },
                    max_mails: None,
                },
                Nanos(11_000_000_000),
            );
            match result {
                DescribeWindowResult::Ok { mails } => {
                    assert_eq!(mails.len(), 2);
                    let ids: std::collections::HashSet<MailId> =
                        mails.iter().map(|m| m.mail_id).collect();
                    assert!(ids.contains(&mail(1, 2)));
                    assert!(ids.contains(&mail(1, 3)));
                }
                DescribeWindowResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn describe_window_empty_when_no_match() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            apply_sent_event(
                &mut obs,
                m,
                m,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Absolute {
                        start_ns: 1_000,
                        end_ns: Some(2_000),
                    },
                    max_mails: None,
                },
                Nanos(3_000),
            );
            match result {
                DescribeWindowResult::Ok { mails } => assert!(mails.is_empty()),
                DescribeWindowResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn retention_evicts_stale() {
            let mut obs = observer_with(Duration::from_millis(50), 1000);
            let m = mail(1, 1);
            apply_sent_event(
                &mut obs,
                m,
                m,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            assert_eq!(obs.roots.len(), 1);
            std::thread::sleep(Duration::from_millis(80));
            obs.evict();
            assert!(obs.roots.is_empty());
            assert!(obs.mails.is_empty());
        }
    }
}
