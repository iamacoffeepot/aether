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
    DispatchTraced, DispatchTracedAck, ListActiveRoots, ListActiveRootsResult, MailNodeWire,
    RootSummaryWire, TraceWindow,
};

#[aether_actor::bridge(singleton)]
mod native {
    use super::{
        BatchedTraceEvents, DescribeTree, DescribeTreeResult, DescribeWindow, DescribeWindowResult,
        DispatchTraced, DispatchTracedAck, ListActiveRoots, ListActiveRootsResult, MailNodeWire,
        RootSummaryWire, TraceWindow,
    };
    use std::cmp::Reverse;
    #[cfg(test)]
    use std::collections::HashSet;
    use std::collections::{BTreeSet, HashMap};
    use std::env;
    use std::mem;
    use std::ops::Bound;
    use std::sync::Arc;
    #[cfg(test)]
    use std::thread;
    use std::time::{Duration, Instant};

    use aether_actor::{MailCtx, actor};
    use aether_data::{KindId, MailId, MailboxId};
    use aether_kinds::trace::{Nanos, Settled, TraceEvent};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::Mail;
    use aether_substrate::mail::helpers::resolve_bundle;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::Registry;

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
    ///
    /// `held_open` tracks ADR-0080 §12 settlement holds — currently
    /// only `InheritCtx<A>` from `NativeCtx::spawn_inherit` produces
    /// these via [`aether_substrate::runtime::trace::acquire_settlement_hold`].
    /// `Settled` emission gates on `(in_flight == 0 && held_open == 0)`
    /// so a worker thread that outlives its spawning handler keeps the
    /// chain open until it drops.
    ///
    /// `settled_at` is `Some` once the root settled — the immutable
    /// timestamp the retention index keys on (iamacoffeepot/aether#1048).
    /// `None` while in-flight; cleared back to `None` if a settled root
    /// is resurrected by a later `Sent`/`HoldOpen`. `last_event_at` is
    /// the *mutable* last-activity stamp, used only by the hard-cap
    /// memory valve (not retention), so it can't index retention.
    #[derive(Debug, Clone)]
    pub struct RootState {
        pub in_flight: u32,
        pub held_open: u32,
        pub last_event_at: Instant,
        pub settled_at: Option<Instant>,
    }

    /// Throttled eviction profiling (iamacoffeepot/aether#1048). The
    /// observer's eviction cost was invisible until it wedged the
    /// lifecycle; this surfaces a periodic heartbeat over `actor_logs
    /// aether.trace` and escalates to `warn` when a single batch's
    /// eviction crosses the frame budget (the direct early-warning for
    /// the settlement-on-critical-path coupling).
    struct EvictStats {
        report_at: Instant,
        max: Duration,
        batches: u64,
        removed: u64,
    }

    impl EvictStats {
        fn new() -> Self {
            Self {
                report_at: Instant::now(),
                max: Duration::ZERO,
                batches: 0,
                removed: 0,
            }
        }
    }

    /// Heartbeat cadence + per-batch budget for [`EvictStats`].
    const EVICT_REPORT_INTERVAL: Duration = Duration::from_secs(10);
    const EVICT_WARN_BUDGET: Duration = Duration::from_millis(5);

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
        /// iamacoffeepot/aether#1048: retention eviction index, keyed on
        /// each root's *settlement* `Instant` (immutable once set). Only
        /// settled roots appear here, so `evict` range-drops the expired
        /// prefix in `O(log n + k)` instead of scanning every root every
        /// batch. In-flight roots are absent → never time-evicted
        /// (ADR-0080 §11 holds for free). `Instant` collisions tie-break
        /// by `MailId`, same as `t_sent_index`.
        evictable: BTreeSet<(Instant, MailId)>,
        /// iamacoffeepot/aether#1048: root → its tracked mail ids, so a
        /// root eviction drops exactly its own mails (`O(k)`) instead of
        /// the prior full `mails` scan. Populated on `TraceEvent::Sent`,
        /// drained in [`Self::remove_root`].
        mails_by_root: HashMap<MailId, Vec<MailId>>,
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
        /// Issue 749: substrate registry handle for `DispatchTraced`'s
        /// per-envelope name resolution (recipient mailbox name → id,
        /// kind name → id). Cloned from `ctx.mailer().registry()` at
        /// init; matches the `RenderCapability` pattern that resolves
        /// `CaptureFrame` mail bundles through the same registry.
        registry: Arc<Registry>,
        /// iamacoffeepot/aether#1048: throttled eviction profiling.
        evict_stats: EvictStats,
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
                        held_open: 0,
                        last_event_at: now,
                        settled_at: None,
                    });
                    // Resurrection: a `Sent` for a root that had already
                    // settled re-opens the chain. Clear the settlement
                    // stamp and drop the now-stale retention index entry
                    // (deferred past the last `root_state` use so the
                    // `self.evictable` borrow doesn't overlap the
                    // `self.roots` borrow). Shouldn't happen under the
                    // exact-settlement hold contract, but the observer is
                    // eventually-consistent over an unordered event stream.
                    let resurrected = root_state.settled_at.take();
                    root_state.in_flight = root_state.in_flight.saturating_add(1);
                    root_state.last_event_at = now;
                    if let Some(at) = resurrected {
                        self.evictable.remove(&(at, root));
                    }
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
                    self.mails_by_root.entry(root).or_default().push(mail_id);
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
                        // ADR-0080 §6 / §12: settlement fires when BOTH
                        // `in_flight` and `held_open` reach zero (the
                        // latter gates thread-spawn primitives — see
                        // iamacoffeepot/aether#716). Compute the verdict,
                        // dropping the `state` borrow before the `&mut
                        // self` `fire_settled` call below.
                        let settled = if let Some(state) = self.roots.get_mut(&root) {
                            state.in_flight = state.in_flight.saturating_sub(1);
                            state.last_event_at = Instant::now();
                            state.in_flight == 0 && state.held_open == 0
                        } else {
                            false
                        };
                        // `fire_settled` pushes `Settled { root }` to
                        // `CHASSIS_MAILBOX_ID` via the bare mailer (so the
                        // outbound generates no trace events) and stamps
                        // the retention index.
                        if settled {
                            self.fire_settled(root);
                        }
                    }
                }
                // ADR-0080 §12 / iamacoffeepot/aether#716: spawn-thread
                // primitives push HoldOpen on acquire and Release on the
                // hold's Drop. Both ride the same trace queue as Sent /
                // Received / Finished so ordering is preserved: a
                // HoldOpen pushed by the parent thread before the worker
                // starts is folded into the observer's state before the
                // parent handler's Finished arrives.
                TraceEvent::HoldOpen { root, t: _ } => {
                    let now = Instant::now();
                    let root_state = self.roots.entry(root).or_insert(RootState {
                        in_flight: 0,
                        held_open: 0,
                        last_event_at: now,
                        settled_at: None,
                    });
                    // Resurrection: a hold acquired against an
                    // already-settled root re-opens the chain (see the
                    // `Sent` branch). Drop the stale retention entry.
                    let resurrected = root_state.settled_at.take();
                    root_state.held_open = root_state.held_open.saturating_add(1);
                    root_state.last_event_at = now;
                    if let Some(at) = resurrected {
                        self.evictable.remove(&(at, root));
                    }
                }
                TraceEvent::Release { root, t: _ } => {
                    // Symmetric with Finished: re-check the joint gate. A
                    // Release that brings held_open to 0 while in_flight
                    // is also 0 fires settlement (the spawned thread
                    // outlived its handler).
                    let settled = if let Some(state) = self.roots.get_mut(&root) {
                        state.held_open = state.held_open.saturating_sub(1);
                        state.last_event_at = Instant::now();
                        state.in_flight == 0 && state.held_open == 0
                    } else {
                        false
                    };
                    if settled {
                        self.fire_settled(root);
                    }
                    // Orphan Release (no matching HoldOpen ever observed
                    // — trace queue not installed when the hold was
                    // acquired, or the root was evicted) is dropped.
                    // Eventual-consistency per ADR-0080 §6.
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
            summaries.sort_by_key(|s| Reverse(s.t_sent));
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

        fn fire_settled(&mut self, root: MailId) {
            // Stamp the immutable settlement time and enrol the root in
            // the retention index (iamacoffeepot/aether#1048). A root can
            // only fire once per settle; a resurrecting `Sent`/`HoldOpen`
            // clears `settled_at` and the index entry before it could
            // fire again.
            let now = Instant::now();
            if let Some(state) = self.roots.get_mut(&root) {
                state.settled_at = Some(now);
            }
            self.evictable.insert((now, root));

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

        /// Removes a root and all of its tracked mails from every index
        /// in one pass (iamacoffeepot/aether#1048). `settled_at` is the
        /// root's recorded settlement stamp when the caller still needs
        /// the retention entry cleaned (`None` when the caller already
        /// drained it, e.g. the retention range-drop). `O(k)` in the
        /// root's mail count — no full `mails` scan.
        fn remove_root(&mut self, root: MailId, settled_at: Option<Instant>) {
            self.roots.remove(&root);
            if let Some(at) = settled_at {
                self.evictable.remove(&(at, root));
            }
            if let Some(mids) = self.mails_by_root.remove(&root) {
                for mid in mids {
                    if let Some(node) = self.mails.remove(&mid) {
                        self.t_sent_index.remove(&(node.t_sent, mid));
                    }
                }
            }
        }

        /// Returns the number of roots evicted this call.
        fn evict(&mut self) -> usize {
            let mut removed = 0usize;

            // Retention (ADR-0080 §11): drop settled roots whose
            // settlement is older than `retention`, via the
            // settlement-time index. `split_off` partitions at the
            // cutoff key — `O(log n)` — leaving only the expired prefix
            // to drain (`O(k)`). No per-batch full scan; in-flight roots
            // aren't in the index, so they're never time-evicted.
            if let Some(cutoff) = Instant::now().checked_sub(self.retention) {
                let fresh = self.evictable.split_off(&(cutoff, MailId::NONE));
                let expired = mem::replace(&mut self.evictable, fresh);
                for (settled_at, root) in expired {
                    let current = self.roots.get(&root).and_then(|s| s.settled_at);
                    // A resurrected-and-resettled root carries a newer
                    // `settled_at`, making this drained entry stale — skip
                    // it (the live root keeps its newer index slot). The
                    // entry is already gone from `evictable` (split_off),
                    // so `remove_root` gets `None` for the index probe.
                    if current == Some(settled_at) {
                        self.remove_root(root, None);
                        removed += 1;
                    }
                }
            }

            // Hard cap (memory valve, distinct from §11 retention): if
            // we still exceed `max_roots`, drop oldest by `last_event_at`
            // regardless of settled state. `O(n log n)` but only on
            // overflow, so it amortises. Evicting an *in-flight* root
            // here means a chain is leaking (missing Finished/Release) or
            // retention can't keep up — surface it loudly rather than
            // silently corrupting the trace graph.
            if self.roots.len() > self.max_roots {
                let mut entries: Vec<(MailId, Instant, Option<Instant>)> = self
                    .roots
                    .iter()
                    .map(|(id, state)| (*id, state.last_event_at, state.settled_at))
                    .collect();
                entries.sort_by_key(|(_, t, _)| *t);
                let drop_n = self.roots.len() - self.max_roots;
                let mut in_flight_evicted = 0u64;
                for (id, _, settled_at) in entries.into_iter().take(drop_n) {
                    if settled_at.is_none() {
                        in_flight_evicted += 1;
                    }
                    self.remove_root(id, settled_at);
                    removed += 1;
                }
                if in_flight_evicted > 0 {
                    tracing::warn!(
                        target: "aether_capabilities::trace",
                        in_flight_evicted,
                        max_roots = self.max_roots,
                        "trace observer hard cap evicted in-flight roots — a chain may be leaking \
                         (missing Finished/Release) or retention isn't keeping up",
                    );
                }
            }

            removed
        }

        /// Folds one batch's eviction outcome into [`EvictStats`] and
        /// emits a throttled heartbeat (iamacoffeepot/aether#1048). The
        /// line escalates to `warn` when a single batch's eviction
        /// crossed [`EVICT_WARN_BUDGET`] — the early-warning the original
        /// wedge lacked. Visible via `actor_logs aether.trace`.
        fn record_evict_sample(&mut self, removed: usize, dur: Duration) {
            self.evict_stats.batches += 1;
            self.evict_stats.removed += removed as u64;
            if dur > self.evict_stats.max {
                self.evict_stats.max = dur;
            }
            if self.evict_stats.report_at.elapsed() < EVICT_REPORT_INTERVAL {
                return;
            }
            let roots = self.roots.len();
            let mails = self.mails.len();
            let evictable = self.evictable.len();
            let max_evict_us = self.evict_stats.max.as_micros();
            let batches = self.evict_stats.batches;
            let removed_total = self.evict_stats.removed;
            if self.evict_stats.max >= EVICT_WARN_BUDGET {
                tracing::warn!(
                    target: "aether_capabilities::trace",
                    roots,
                    mails,
                    evictable,
                    batches,
                    removed = removed_total,
                    max_evict_us,
                    "trace observer eviction exceeded frame budget — settlement latency at risk \
                     (see iamacoffeepot/aether#1048)",
                );
            } else {
                tracing::debug!(
                    target: "aether_capabilities::trace",
                    roots,
                    mails,
                    evictable,
                    batches,
                    removed = removed_total,
                    max_evict_us,
                    "trace observer eviction stats",
                );
            }
            self.evict_stats = EvictStats::new();
        }
    }

    fn parse_env_u64(name: &str, default: u64) -> u64 {
        env::var(name)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }

    fn parse_env_usize(name: &str, default: usize) -> usize {
        env::var(name)
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
            let mailer = ctx.mailer();
            let registry = Arc::clone(mailer.registry());
            Ok(Self {
                roots: HashMap::new(),
                mails: HashMap::new(),
                t_sent_index: BTreeSet::new(),
                evictable: BTreeSet::new(),
                mails_by_root: HashMap::new(),
                retention: Duration::from_millis(retention_ms),
                max_roots,
                mailer,
                settled_kind: <Settled as aether_data::Kind>::ID,
                registry,
                evict_stats: EvictStats::new(),
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
            let evict_start = Instant::now();
            let removed = self.evict();
            self.record_evict_sample(removed, evict_start.elapsed());
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
            let now = ctx.mailer().now_nanos();
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
            let now = ctx.mailer().now_nanos();
            let result = self.build_describe_window(request, now);
            ctx.reply(&result);
        }

        /// # Agent
        /// Atomic batched dispatch with shared trace root, backing the
        /// MCP `send_mail_traced` tool. Captures this handler's inbound
        /// `MailId` as the batch root, dispatches every spec inheriting
        /// the chain (so all children appear under one tree), and
        /// replies synchronously with [`DispatchTracedAck`] carrying
        /// the root. The caller waits for the wire `ReplyEnd` (chain
        /// settled) and then issues a follow-up [`DescribeTree`] for
        /// the populated tree. Issue 749.
        #[handler]
        fn on_dispatch_traced(&mut self, ctx: &mut NativeCtx<'_>, batch: DispatchTraced) {
            let root = ctx.in_flight_mail_id();
            let DispatchTraced { mails } = batch;
            // Resolve every envelope's name addressing through the
            // substrate registry — same path `CaptureFrame`'s bundle
            // resolution uses (`render::on_capture_frame`). A single
            // unresolved name aborts the whole batch, surfaced as the
            // ack's `Err` variant so the MCP caller fails fast.
            let resolved = match resolve_bundle(&self.registry, &mails, "dispatch_traced batch") {
                Ok(v) => v,
                Err(error) => {
                    ctx.reply(&DispatchTracedAck::Err { error });
                    return;
                }
            };
            for mail in resolved {
                let _ = ctx.send_envelope_traced(mail.recipient, mail.kind, &mail.payload);
            }
            ctx.reply(&DispatchTracedAck::Ok { root });
        }
    }

    #[cfg(test)]
    // Tests hold the capture `Mutex` guard across the assertion block
    // so the snapshot reads atomically against the concurrent
    // observer-side push.
    #[allow(clippy::significant_drop_tightening)]
    mod tests {
        use super::*;
        use aether_data::{SessionToken, Uuid};
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
        use aether_substrate::mail::registry::{MailDispatch, Registry};
        use aether_substrate::mail::{ReplyTarget, ReplyTo};
        use std::sync::Mutex;
        use std::sync::mpsc::Receiver;

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
                env::set_var("AETHER_TRACE_RETENTION_MS", "60000");
                env::set_var("AETHER_TRACE_MAX_ROOTS", "1000");
            }
            observer_with(Duration::from_mins(1), 1000)
        }

        fn observer_with(retention: Duration, max_roots: usize) -> TraceObserverCapability {
            let registry = Arc::new(Registry::new());
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
            TraceObserverCapability {
                roots: HashMap::new(),
                mails: HashMap::new(),
                t_sent_index: BTreeSet::new(),
                evictable: BTreeSet::new(),
                mails_by_root: HashMap::new(),
                retention,
                max_roots,
                mailer,
                settled_kind: <Settled as aether_data::Kind>::ID,
                registry,
                evict_stats: EvictStats::new(),
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
            let mut obs = observer_with(Duration::from_hours(1), 3);
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
                thread::sleep(Duration::from_millis(2));
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
            let (mut obs, captured, root) = observer_with_settled_capture();
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
            assert!(captured.lock().expect("captured mutex").is_empty());
            obs.apply_event(TraceEvent::Finished {
                mail_id: root,
                t: Nanos(200),
            });
            // Settled fired; chassis-router decoded the mail.
            let captured = captured.lock().expect("captured mutex");
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].root, root);
        }

        /// ADR-0080 §12 / iamacoffeepot/aether#716 — shared fixture
        /// for settlement-hold tests. Returns the observer +
        /// `Mutex<Vec<Settled>>` capture (chassis-router-routed) +
        /// the chassis-root `MailId` so the tests can apply events
        /// against the same root the capture sees.
        fn observer_with_settled_capture()
        -> (TraceObserverCapability, Arc<Mutex<Vec<Settled>>>, MailId) {
            let registry = Arc::new(Registry::new());
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
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

            let obs = TraceObserverCapability {
                roots: HashMap::new(),
                mails: HashMap::new(),
                t_sent_index: BTreeSet::new(),
                evictable: BTreeSet::new(),
                mails_by_root: HashMap::new(),
                retention: Duration::from_mins(1),
                max_roots: 1000,
                mailer: Arc::clone(&mailer),
                settled_kind,
                registry: Arc::clone(&registry),
                evict_stats: EvictStats::new(),
            };
            (obs, captured, mail(1, 1))
        }

        /// Spawn completes before handler returns: `HoldOpen` +
        /// `Release` land before `Finished`. Settlement fires when
        /// `Finished` drops `in_flight` to 0 because `held_open` is
        /// already 0 by then.
        #[test]
        fn hold_acquired_then_released_before_finished() {
            let (mut obs, captured, root) = observer_with_settled_capture();

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
            obs.apply_event(TraceEvent::HoldOpen {
                root,
                t: Nanos(150),
            });
            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(160),
            });
            // in_flight=1, held_open=0 — Release on its own doesn't fire.
            assert!(
                captured.lock().expect("captured mutex").is_empty(),
                "Release with in_flight > 0 must not fire Settled"
            );

            obs.apply_event(TraceEvent::Finished {
                mail_id: root,
                t: Nanos(200),
            });
            let captured = captured.lock().expect("captured mutex");
            assert_eq!(captured.len(), 1, "Finished with held_open=0 fires");
            assert_eq!(captured[0].root, root);
        }

        /// The iamacoffeepot/aether#716 bug: spawn outlives handler.
        /// `Finished` fires before `Release`. With the gate the
        /// `Finished` event must NOT trigger `Settled`; only the
        /// subsequent `Release` (which drops `held_open` to 0 while
        /// `in_flight` is already 0) fires settlement.
        #[test]
        fn hold_outlives_finished_blocks_then_release_fires_settled() {
            let (mut obs, captured, root) = observer_with_settled_capture();

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
            obs.apply_event(TraceEvent::HoldOpen {
                root,
                t: Nanos(150),
            });
            obs.apply_event(TraceEvent::Finished {
                mail_id: root,
                t: Nanos(200),
            });

            // The bug: in_flight=0 but held_open=1, so Settled must NOT
            // have fired yet. Pre-fix this assertion would fail (Settled
            // would have been captured).
            assert!(
                captured.lock().expect("captured mutex").is_empty(),
                "Settled must be gated by held_open > 0"
            );

            // Worker thread "drops" — Release event arrives.
            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(300),
            });
            let captured = captured.lock().expect("captured mutex");
            assert_eq!(
                captured.len(),
                1,
                "Release with in_flight=0 fires the previously-gated Settled"
            );
            assert_eq!(captured[0].root, root);
        }

        /// Multiple concurrent spawns: each `InheritCtx` acquires its
        /// own hold; the observer accumulates `held_open` to N. Only
        /// the last Release brings both counters to zero, firing
        /// exactly one Settled.
        #[test]
        fn multiple_holds_each_gate_settlement() {
            let (mut obs, captured, root) = observer_with_settled_capture();

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
            obs.apply_event(TraceEvent::HoldOpen {
                root,
                t: Nanos(110),
            });
            obs.apply_event(TraceEvent::HoldOpen {
                root,
                t: Nanos(120),
            });
            obs.apply_event(TraceEvent::HoldOpen {
                root,
                t: Nanos(130),
            });
            assert_eq!(
                obs.roots.get(&root).expect("root").held_open,
                3,
                "three holds → counter at 3"
            );

            obs.apply_event(TraceEvent::Finished {
                mail_id: root,
                t: Nanos(200),
            });
            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(210),
            });
            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(220),
            });
            // Two of three holds released; held_open=1, in_flight=0.
            assert!(
                captured.lock().expect("captured mutex").is_empty(),
                "partial release does not fire Settled"
            );

            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(230),
            });
            let captured = captured.lock().expect("captured mutex");
            assert_eq!(captured.len(), 1, "last Release fires exactly one Settled");
            assert_eq!(captured[0].root, root);
        }

        /// `HoldOpen` for a never-seen root creates the `RootState`
        /// entry with `in_flight = 0`. A subsequent `Sent` ticks
        /// `in_flight` up alongside the existing `held_open`. Order
        /// `Sent`-before-`HoldOpen` vs `HoldOpen`-before-`Sent` is
        /// producer-order-sensitive in practice but the observer is
        /// symmetric.
        #[test]
        fn hold_open_creates_root_state_with_zero_in_flight() {
            let mut obs = boot_observer();
            let root = mail(1, 1);
            obs.apply_event(TraceEvent::HoldOpen { root, t: Nanos(50) });
            let state = obs.roots.get(&root).expect("HoldOpen creates root");
            assert_eq!(state.in_flight, 0);
            assert_eq!(state.held_open, 1);
        }

        /// Orphan `Release` (no matching `HoldOpen` — chassis didn't
        /// have the trace queue installed when the hold was acquired,
        /// or the root was evicted) drops silently. No state change,
        /// no panic.
        #[test]
        fn orphan_release_drops_silently() {
            let mut obs = boot_observer();
            let root = mail(7, 7);
            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(100),
            });
            assert!(obs.roots.is_empty(), "orphan Release does not create state");
            assert!(obs.mails.is_empty());
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
                    let ids: HashSet<MailId> = mails.iter().map(|m| m.mail_id).collect();
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
            // Post-#1048: retention drops settled roots only — settle it.
            obs.apply_event(TraceEvent::Finished {
                mail_id: m,
                t: Nanos(200),
            });
            assert_eq!(obs.t_sent_index.len(), 1);
            thread::sleep(Duration::from_millis(80));
            obs.evict();
            assert!(obs.t_sent_index.is_empty(), "secondary index out of sync");
        }

        #[test]
        fn t_sent_index_drops_on_max_roots_overflow() {
            // Exceeding `max_roots` evicts the oldest by `last_event_at`,
            // and `drop_orphaned_mails` should prune the secondary
            // index for the evicted mails too.
            let mut obs = observer_with(Duration::from_hours(1), 3);
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
                thread::sleep(Duration::from_millis(2));
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
                    let ids: HashSet<MailId> = mails.iter().map(|m| m.mail_id).collect();
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
            // Post-#1048: retention evicts SETTLED roots only (ADR-0080
            // §11 — in-flight roots are never time-evicted). Settle the
            // root so it enters the settlement-time index before the
            // retention window elapses.
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
            obs.apply_event(TraceEvent::Finished {
                mail_id: m,
                t: Nanos(200),
            });
            assert_eq!(obs.roots.len(), 1);
            thread::sleep(Duration::from_millis(80));
            obs.evict();
            assert!(obs.roots.is_empty());
            assert!(obs.mails.is_empty());
        }

        /// ADR-0080 §11 / iamacoffeepot/aether#1048: an in-flight root
        /// (never `Finished`) is **never** time-evicted, no matter how
        /// long it sits past the retention window — it isn't in the
        /// settlement-time index. The prior `retain(last_event_at >=
        /// cutoff)` violated this; the wedge fix makes §11 structural.
        #[test]
        fn retention_never_evicts_in_flight_root() {
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
            thread::sleep(Duration::from_millis(80));
            let removed = obs.evict();
            assert_eq!(removed, 0, "in-flight root must survive retention");
            assert_eq!(obs.roots.len(), 1, "in-flight root must not be evicted");
            assert_eq!(obs.mails.len(), 1);
        }

        /// iamacoffeepot/aether#1048 regression: under the wedge-causing
        /// load shape (a steady stream of settle-then-evict batches), the
        /// working set stays bounded and per-batch eviction work stays
        /// flat — it does **not** grow with cumulative root count. The
        /// prior `evict` scanned every root every batch and (with the
        /// 10-minute default retention) evicted nothing for minutes, so
        /// state — and scan cost — grew without bound until settlement
        /// latency exceeded the frame tick. Here a 1 ms retention plus a
        /// 2 ms inter-batch sleep guarantees the prior batch is always
        /// expired by the next `evict`, so the working set never exceeds
        /// ~one batch. `thread::sleep` only ever over-sleeps, so the
        /// bound is robust to CI jitter in both directions (a longer
        /// sleep just expires more, and we drain every batch).
        #[test]
        fn soak_settled_roots_stay_bounded_with_flat_evict_work() {
            const BATCHES: usize = 60;
            const PER_BATCH: usize = 50;

            let mut obs = observer_with(Duration::from_millis(1), 1_000_000);
            let mut max_roots_seen = 0usize;
            let mut max_removed_in_one_evict = 0usize;
            let mut total_settled = 0usize;

            for b in 0..BATCHES {
                // Each "batch" sends + finishes PER_BATCH distinct roots
                // (the advance-shaped Sent→Finished pair), then evicts —
                // exactly the observer's per-batch hot path.
                for i in 0..PER_BATCH {
                    let cid = (b * PER_BATCH + i + 1) as u64;
                    let m = mail(1, cid);
                    apply_sent_event(
                        &mut obs,
                        m,
                        m,
                        None,
                        MailboxId(1),
                        MailboxId(2),
                        KindId(0xABCD),
                        Nanos(cid),
                    );
                    obs.apply_event(TraceEvent::Finished {
                        mail_id: m,
                        t: Nanos(cid),
                    });
                    total_settled += 1;
                }
                max_roots_seen = max_roots_seen.max(obs.roots.len());
                let removed = obs.evict();
                max_removed_in_one_evict = max_removed_in_one_evict.max(removed);
                // Sleep > retention so the *previous* batch is expired by
                // the next evict; the working set stays at ~one batch.
                thread::sleep(Duration::from_millis(2));
            }

            // Working set never approaches the cumulative total — old
            // behaviour would have grown `roots` to all BATCHES*PER_BATCH.
            let cumulative = BATCHES * PER_BATCH;
            assert!(
                max_roots_seen <= PER_BATCH * 5 && max_roots_seen < cumulative / 4,
                "working set {max_roots_seen} not bounded vs cumulative {cumulative}"
            );
            // Per-batch eviction work is proportional to what expired (k),
            // not total state (n) — a small multiple of one batch.
            assert!(
                max_removed_in_one_evict <= PER_BATCH * 5,
                "single-batch eviction {max_removed_in_one_evict} not bounded by recent inflow"
            );

            // Final flush: let the last batch expire and drain. After a
            // full eviction every index is mutually consistent — no leak.
            thread::sleep(Duration::from_millis(5));
            obs.evict();
            assert!(obs.roots.is_empty(), "all settled roots eventually evict");
            assert!(obs.mails.is_empty(), "mails drained with their roots");
            assert!(obs.t_sent_index.is_empty(), "t_sent_index stays in sync");
            assert!(obs.evictable.is_empty(), "evictable index stays in sync");
            assert!(
                obs.mails_by_root.is_empty(),
                "root->mails index stays in sync"
            );
            assert!(total_settled > 0);
        }

        /// Shared scaffolding for the `on_dispatch_traced` tests:
        /// fresh registry + mailer + outbound + transport + observer
        /// wired together with the `mailer.outbound -> rx` egress
        /// recorder. The observer doesn't go through `init` (which
        /// reads env vars and ctx state); construct it directly with
        /// the registry handle the resolve path needs.
        struct DispatchTracedFixture {
            registry: Arc<Registry>,
            rx: Receiver<EgressEvent>,
            transport: Arc<NativeBinding>,
            cap: TraceObserverCapability,
        }

        fn dispatch_traced_fixture() -> DispatchTracedFixture {
            let registry = Arc::new(Registry::new());
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let (outbound, rx) = HubOutbound::attached_loopback();
            let mailer = Arc::new(
                Mailer::new(Arc::clone(&registry), Arc::clone(&store)).with_outbound(outbound),
            );
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0x7ACE),
            ));
            let cap = TraceObserverCapability {
                roots: HashMap::new(),
                mails: HashMap::new(),
                t_sent_index: BTreeSet::new(),
                evictable: BTreeSet::new(),
                mails_by_root: HashMap::new(),
                retention: Duration::from_mins(1),
                max_roots: 1000,
                mailer,
                settled_kind: <Settled as aether_data::Kind>::ID,
                registry: Arc::clone(&registry),
                evict_stats: EvictStats::new(),
            };
            DispatchTracedFixture {
                registry,
                rx,
                transport,
                cap,
            }
        }

        /// Build a chassis-root `NativeCtx` against the fixture's
        /// transport, anchoring the in-flight + reply-to fields to a
        /// session sender so the ack reply egresses as `ToSession`.
        fn chassis_root_ctx(transport: &Arc<NativeBinding>, inbound: MailId) -> NativeCtx<'_> {
            let sender = ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::nil())));
            NativeCtx::new(transport, sender, inbound, inbound)
        }

        /// Drain `rx` until it goes quiet, decoding every
        /// `DispatchTracedAck` `ToSession` egress it sees. Exactly
        /// one ack is the success shape; the test asserts on `len()`.
        fn drain_ack_replies(rx: &Receiver<EgressEvent>) -> Vec<DispatchTracedAck> {
            let mut acks = Vec::new();
            while let Ok(event) = rx.recv_timeout(Duration::from_millis(250)) {
                if let EgressEvent::ToSession {
                    kind_name, payload, ..
                } = event
                    && kind_name == <DispatchTracedAck as aether_data::Kind>::NAME
                {
                    acks.push(postcard::from_bytes(&payload).expect("ack payload decodes"));
                }
            }
            acks
        }

        /// Issue 749: `on_dispatch_traced` resolves each envelope's
        /// name addressing through the registry (matching
        /// `CaptureFrame`'s bundle pattern), dispatches each via
        /// `send_envelope_traced` so children inherit the chain, and
        /// replies synchronously with `DispatchTracedAck::Ok { root }`
        /// carrying the inbound mail id.
        #[test]
        fn on_dispatch_traced_resolves_each_envelope_and_acks_with_root() {
            use aether_kinds::MailEnvelope;
            use std::sync::Mutex;

            type Capture = (KindId, MailId, Option<MailId>, Vec<u8>);

            /// Inline handler that records every dispatched mail's
            /// `(kind, root, parent, payload)` into the shared
            /// `Vec`. Used twice to register two stub recipients.
            fn register_capture(registry: &Registry, name: &str, sink: Arc<Mutex<Vec<Capture>>>) {
                registry.register_inline(
                    name,
                    Arc::new(move |d: MailDispatch<'_>| {
                        sink.lock()
                            .expect("test stub: captured mutex poisoned")
                            .push((d.kind, d.root, d.parent_mail, d.payload.to_vec()));
                    }),
                );
            }

            let mut fix = dispatch_traced_fixture();
            // Resolve_bundle needs both mailbox (by name) and kind to
            // be registered, else it short-circuits with the early-
            // abort `Err` path the other test exercises.
            let captured: Arc<Mutex<Vec<Capture>>> = Arc::new(Mutex::new(Vec::new()));
            register_capture(&fix.registry, "aether.test.spec_a", Arc::clone(&captured));
            register_capture(&fix.registry, "aether.test.spec_b", Arc::clone(&captured));
            let kind_alpha = fix.registry.register_kind("aether.test.kind_a");
            let kind_beta = fix.registry.register_kind("aether.test.kind_b");

            let inbound = MailId::new(MailboxId(0xC0DE), 7);
            let mut ctx = chassis_root_ctx(&fix.transport, inbound);
            fix.cap.on_dispatch_traced(
                &mut ctx,
                DispatchTraced {
                    mails: vec![
                        MailEnvelope {
                            recipient_name: "aether.test.spec_a".into(),
                            kind_name: "aether.test.kind_a".into(),
                            payload: vec![1u8, 2],
                            count: 1,
                        },
                        MailEnvelope {
                            recipient_name: "aether.test.spec_b".into(),
                            kind_name: "aether.test.kind_b".into(),
                            payload: vec![3u8, 4, 5],
                            count: 1,
                        },
                    ],
                },
            );

            let snapshot = captured
                .lock()
                .expect("test stub: captured mutex poisoned")
                .clone();
            assert_eq!(snapshot.len(), 2, "expected each envelope to dispatch");
            assert!(
                snapshot.iter().any(|(k, root, parent, p)| *k == kind_alpha
                    && *root == inbound
                    && *parent == Some(inbound)
                    && p == &vec![1u8, 2]),
                "envelope A missing or chain not inherited; captured: {snapshot:?}"
            );
            assert!(
                snapshot.iter().any(|(k, root, parent, p)| *k == kind_beta
                    && *root == inbound
                    && *parent == Some(inbound)
                    && p == &vec![3u8, 4, 5]),
                "envelope B missing or chain not inherited; captured: {snapshot:?}"
            );

            let acks = drain_ack_replies(&fix.rx);
            assert_eq!(acks.len(), 1, "expected exactly one ack reply");
            match &acks[0] {
                DispatchTracedAck::Ok { root } => assert_eq!(
                    *root, inbound,
                    "Ok ack must echo the in-flight inbound mail id as the chassis root"
                ),
                DispatchTracedAck::Err { error } => {
                    panic!("expected Ok ack, got Err: {error}")
                }
            }
        }

        /// Issue 749: an unresolvable name in the batch short-circuits
        /// to `DispatchTracedAck::Err`; no envelope dispatches.
        #[test]
        fn on_dispatch_traced_replies_err_on_unknown_recipient() {
            use aether_kinds::MailEnvelope;

            let mut fix = dispatch_traced_fixture();
            let inbound = MailId::new(MailboxId(0xC0DE), 99);
            let mut ctx = chassis_root_ctx(&fix.transport, inbound);
            fix.cap.on_dispatch_traced(
                &mut ctx,
                DispatchTraced {
                    mails: vec![MailEnvelope {
                        recipient_name: "aether.test.does_not_exist".into(),
                        kind_name: "aether.test.also_missing".into(),
                        payload: vec![],
                        count: 1,
                    }],
                },
            );

            let acks = drain_ack_replies(&fix.rx);
            assert_eq!(acks.len(), 1);
            assert!(
                matches!(&acks[0], DispatchTracedAck::Err { error } if error.contains("unknown recipient")),
                "expected Err with 'unknown recipient' message, got: {:?}",
                acks[0]
            );
        }
    }
}
