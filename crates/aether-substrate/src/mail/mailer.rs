// Mailer locks are held across the lookup + dispatch pair on
// purpose — dropping the guard between resolving the recipient and
// pushing the envelope would open a TOCTOU window where the mailbox
// could be unregistered or replaced mid-call.
#![allow(clippy::significant_drop_tightening)]

// Inline router (ADR-0038 Phase 3 + issue 603).
//
// Phase 2 retired the VecDeque + router thread; Phase 3 retired the
// global `outstanding` / `done_cv` barrier in favour of per-component
// drains. Issue 603 retired the shared `ComponentTable` Arc.
//
// Issue 634 Phase 4 retired the wasm-component-specific routing path
// entirely: every loaded wasm component is now a `WasmTrampoline`
// `NativeActor` registered as a `MailboxEntry::Inbox` like every
// other actor, so the dedicated `ComponentRouter` slot + `route()`
// method + `MailboxEntry::Component` variant are gone. PR 2 retired
// the `drain_all_with_budget` polling barrier in favour of direct
// trap-abort at the trampoline (the trampoline holds a
// `FatalAborter` and aborts on `Component::deliver` Err).
//
// `push(mail)` still resolves the recipient inline on the caller's
// thread.

use std::borrow::Cow;
use std::sync::Arc;
use std::thread;

use crate::chassis::settlement::SettlementRegistry;
use crate::handle_store::{self, HandleStore, PutError, WalkOutcome};
use crate::mail::capability::CapabilityRegistry;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{MailDispatch, MailboxEntry, OwnedDispatch, Registry};
use crate::mail::{Mail, ReplyTarget, ReplyTo};
use crate::runtime::trace::{SettlementHold, TraceHandle};
use aether_data::{HandleId, KindId};
use aether_kinds::trace::Nanos;
use std::sync::OnceLock;

pub struct Mailer {
    /// Registry handle for resolving recipients on `push`. Owned for
    /// the `Mailer`'s lifetime; supplied at construction time alongside
    /// the `HandleStore` (issue 657 collapsed the prior `wire`-after-
    /// `new` setter pair into a required-pair constructor).
    registry: Arc<Registry>,
    /// ADR-0045 typed-handle resolver. `route_mail` runs each mail
    /// through the ref-walker before dispatching; missing handles
    /// park the mail in the store. Required at construction time —
    /// `SubstrateBoot::build` builds one with `HandleStore::from_env()`,
    /// tests pass `Arc::new(HandleStore::new(1024 * 1024))`. Schemas
    /// with no `Ref` nodes hit the no-op fast path inside `route_mail`,
    /// so passing a store on tests that don't exercise handles costs
    /// nothing.
    handle_store: Arc<HandleStore>,
    /// Hub outbound handle. When set and connected, mail to unknown
    /// mailbox ids bubbles up to the hub-substrate (ADR-0037
    /// Phase 1) instead of being warn-dropped locally. Optional —
    /// chassis that skip hub connection (today: the hub chassis
    /// itself) construct a `Mailer` without an outbound and keep
    /// local warn-drop semantics intact (the hub is the end of the
    /// bubbles-up line). Pre-issue-657 this rode an `OnceLock` set
    /// post-construction via `wire_outbound`; the constructor +
    /// `with_outbound` pair below replaces that.
    outbound: Option<Arc<HubOutbound>>,
    /// ADR-0080 §5 chassis-mail router. When mail is addressed to
    /// [`MailboxId::CHASSIS_MAILBOX_ID`], `route_mail` short-circuits
    /// the registry lookup and invokes this closure instead. Today
    /// the chassis installs a router that decodes `Settled { root }`
    /// and signals the [`SettlementRegistry`]
    /// — keeping the Mailer ignorant of trace kinds while still
    /// providing the dispatch surface those kinds need.
    ///
    /// `OnceLock` so the chassis builder installs exactly once at
    /// boot. `None` for tests / chassis that don't bring up the
    /// trace pipeline — chassis-addressed mail is silently dropped
    /// in that case.
    chassis_router: OnceLock<Box<dyn Fn(Mail) + Send + Sync>>,
    /// ADR-0080 §6 settlement registry handle, exposed so capabilities
    /// hosting external entry points (`RpcServer`, future event-source
    /// caps) can subscribe to settlement of mail they dispatch from
    /// their handlers. Threaded through the Mailer rather than down
    /// every `NativeBinding` because settlement is a chassis-wide
    /// service used at runtime — the cap reaches it via
    /// `ctx.mailer().settlement_registry()`.
    ///
    /// `OnceLock` so the chassis builder installs exactly once at
    /// boot alongside [`Self::chassis_router`]. `None` on test fixtures
    /// / chassis that don't bring up the trace pipeline; callers that
    /// expect to subscribe must check for the presence and surface a
    /// clear error otherwise.
    settlement_registry: OnceLock<Arc<SettlementRegistry>>,
    /// ADR-0080 per-chassis trace handle. Holds the trace event queue
    /// and the boot-time anchor for `Nanos` timestamps. Producer-side
    /// hooks (`record_sent` / `record_received` / `record_finished`,
    /// `acquire_settlement_hold`) reach for it via the shortcut
    /// methods on this `Mailer`.
    ///
    /// Per-`Mailer` (not process-global) since iamacoffeepot/aether#953
    /// — see the module doc on [`crate::runtime::trace`]. Always
    /// present (allocated by [`Self::new`] with a fresh `SegQueue` +
    /// boot anchor); chassis that want to share a queue across
    /// multiple `Mailer`s swap in via [`Self::with_trace_handle`]
    /// before the `Arc` wrap. Tests get a real handle for free;
    /// `start_drainer` is independent — without it, events accumulate
    /// in the queue but aren't shipped.
    trace_handle: TraceHandle,
    /// iamacoffeepot/aether#1037 queryable capability registry. A
    /// sibling of [`Self::registry`] — the routing registry resolves
    /// recipients on the hot path; this one answers the DAG
    /// validator's submit-path dispatchability questions
    /// (`accepts(MailboxId, KindId)` / `has_fallback(MailboxId)`). The
    /// component-load / native-cap-boot path populates it, replace
    /// re-registers, drop clears. Allocated empty by [`Self::new`]
    /// (like `trace_handle`) so no call site changes.
    capability_registry: Arc<CapabilityRegistry>,
}

impl Mailer {
    /// Construct a `Mailer` against the substrate's registry and
    /// handle store. `SubstrateBoot::build` is the production caller;
    /// tests build the same trio with `Registry::new()` and
    /// `HandleStore::new(1024 * 1024)` (or any byte budget). Call
    /// [`Self::with_outbound`] to attach a hub outbound if the
    /// chassis needs ADR-0037 bubble-up.
    pub fn new(registry: Arc<Registry>, handle_store: Arc<HandleStore>) -> Self {
        Self {
            registry,
            handle_store,
            outbound: None,
            chassis_router: OnceLock::new(),
            settlement_registry: OnceLock::new(),
            trace_handle: TraceHandle::new(),
            capability_registry: Arc::new(CapabilityRegistry::new()),
        }
    }

    /// ADR-0080 §5 chassis-mail router installation. Called once by
    /// the chassis builder at boot to wire the closure that handles
    /// mail addressed to [`aether_data::MailboxId::CHASSIS_MAILBOX_ID`].
    /// Subsequent calls are no-ops — the router slot is single-claim.
    pub fn install_chassis_router(&self, router: Box<dyn Fn(Mail) + Send + Sync>) {
        let _ = self.chassis_router.set(router);
    }

    /// ADR-0080 §6 settlement-registry installation. Called once by the
    /// chassis builder at boot alongside [`Self::install_chassis_router`]
    /// so capability handlers can reach the registry via
    /// `ctx.mailer().settlement_registry()`. Single-claim; subsequent
    /// calls are no-ops.
    pub fn install_settlement_registry(&self, registry: Arc<SettlementRegistry>) {
        let _ = self.settlement_registry.set(registry);
    }

    /// Borrow the wired [`SettlementRegistry`], or `None` if no
    /// registry was installed (test fixtures, chassis
    /// that don't bring up the trace pipeline). Capabilities subscribe
    /// via
    /// [`SettlementRegistry::subscribe_settlement_mail`].
    pub fn settlement_registry(&self) -> Option<&Arc<SettlementRegistry>> {
        self.settlement_registry.get()
    }

    /// Swap in a non-default [`TraceHandle`].
    /// Production chassis use the default handle that [`Self::new`]
    /// allocates (fresh `SegQueue` + boot anchor). Tests that need
    /// multiple `Mailer`s to share a queue construct one with
    /// [`TraceHandle::with_queue`] and pass
    /// it here before the `Arc` wrap. Filed under
    /// iamacoffeepot/aether#953 — per-chassis trace state.
    #[must_use]
    pub fn with_trace_handle(mut self, handle: TraceHandle) -> Self {
        self.trace_handle = handle;
        self
    }

    /// Borrow the [`TraceHandle`]. Always
    /// present — `Mailer::new` allocates a default handle, and
    /// chassis swap in via [`Self::with_trace_handle`] if they want a
    /// non-default queue. Producer-side call sites usually reach for
    /// the shortcut methods on this `Mailer` instead (`record_sent` /
    /// `record_received` / `record_finished` /
    /// `acquire_settlement_hold` / `now_nanos`).
    pub fn trace_handle(&self) -> &TraceHandle {
        &self.trace_handle
    }

    /// ADR-0080 §2 producer hook for the `Sent` event. Always pushes
    /// — every `Mailer` carries a trace handle; the drainer is the
    /// optional piece (without [`crate::runtime::trace::start_drainer`]
    /// events accumulate in the queue but aren't shipped).
    pub fn record_sent(
        &self,
        mail_id: aether_data::MailId,
        root: aether_data::MailId,
        parent_mail: Option<aether_data::MailId>,
        sender: aether_data::MailboxId,
        recipient: aether_data::MailboxId,
        kind: KindId,
    ) {
        self.trace_handle
            .record_sent(mail_id, root, parent_mail, sender, recipient, kind);
    }

    /// ADR-0080 §2 producer hook for the `Received` event.
    pub fn record_received(&self, mail_id: aether_data::MailId, thread_name: Option<String>) {
        self.trace_handle.record_received(mail_id, thread_name);
    }

    /// ADR-0080 §2 producer hook for the `Finished` event.
    pub fn record_finished(&self, mail_id: aether_data::MailId) {
        self.trace_handle.record_finished(mail_id);
    }

    /// ADR-0080 §12 / iamacoffeepot/aether#716: acquire a settlement
    /// hold on `root`. The returned guard fires `Release` on drop;
    /// every `Mailer` carries a real handle so the contract is
    /// structural.
    #[must_use = "SettlementHold gates root settlement; storing _ silently fires Release"]
    pub fn acquire_settlement_hold(&self, root: aether_data::MailId) -> SettlementHold {
        self.trace_handle.acquire_settlement_hold(root)
    }

    /// Current monotonic `Nanos` timestamp relative to the trace
    /// handle's boot anchor.
    #[must_use]
    pub fn now_nanos(&self) -> Nanos {
        self.trace_handle.now_nanos()
    }

    /// ADR-0080 chassis-root push helper. Combines `MailId` minting,
    /// the `Sent` trace event emission, and the [`Mailer::push`]
    /// into one call so chassis-side mail (Tick fanout from the
    /// frame loop, hub-bridged inbound, MCP-bridged) gets observable
    /// lineage without duplicating the producer-side hook in
    /// `NativeBinding::send_mail_with_lineage`.
    ///
    /// Returns the freshly minted `MailId` so the caller can
    /// subscribe to its settlement via the chassis
    /// [`SettlementRegistry`] before
    /// waiting on the chain.
    ///
    /// `correlation_id` is allocated by the caller (the chassis
    /// owns its own `AtomicU64` counter, symmetric with each
    /// per-actor `NativeBinding`'s counter).
    pub fn push_chassis_root_mail(
        &self,
        correlation_id: u64,
        recipient: aether_data::MailboxId,
        kind: KindId,
        payload: Vec<u8>,
        count: u32,
    ) -> aether_data::MailId {
        let mail_id =
            aether_data::MailId::new(aether_data::MailboxId::CHASSIS_MAILBOX_ID, correlation_id);
        self.record_sent(
            mail_id,
            mail_id,
            None,
            aether_data::MailboxId::CHASSIS_MAILBOX_ID,
            recipient,
            kind,
        );
        self.push(Mail::new(recipient, kind, payload, count).with_lineage(mail_id, mail_id, None));
        mail_id
    }

    /// Attach a `HubOutbound` so mail to unknown mailbox ids bubbles
    /// up to the hub-substrate (ADR-0037 Phase 1) instead of being
    /// warn-dropped. Fluent — returns `self` so the call site can
    /// chain after `Mailer::new`. Skip the call entirely for chassis
    /// that are their own hub or for tests that want local warn-drop
    /// semantics (the hub chassis, the warn-drop test in
    /// `actor::wasm::component`).
    #[must_use]
    pub fn with_outbound(mut self, outbound: Arc<HubOutbound>) -> Self {
        self.outbound = Some(outbound);
        self
    }

    /// Borrow the wired `HandleStore`. Read-only handle exposed so
    /// chassis-side handlers (PR 3 host-fn shims) can publish into
    /// the same store the dispatch path resolves against.
    pub fn handle_store(&self) -> &Arc<HandleStore> {
        &self.handle_store
    }

    /// Borrow the wired [`HubOutbound`], or `None` if no outbound was
    /// attached (the hub chassis, or tests). Surfaced for chassis caps
    /// that thread egress events (replies, log batches, etc.) back to
    /// the hub without the substrate holding a registry closure for
    /// them.
    pub fn outbound(&self) -> Option<&Arc<HubOutbound>> {
        self.outbound.as_ref()
    }

    /// Borrow the wired [`Registry`]. Issue 603: surfaced so
    /// `ComponentHostCapability::init` can pull the registry for its
    /// internal state without requiring it on `ComponentHostConfig` —
    /// per Resolved Decision §2 registry arrives via init ctx, not
    /// via the cap's config struct.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Borrow the wired [`CapabilityRegistry`]
    /// (iamacoffeepot/aether#1037). The component-load / native-cap-boot
    /// path registers mailbox caps through this handle; the DAG
    /// validator (iamacoffeepot/aether#975) reads `accepts` /
    /// `has_fallback` on the submit path. Shared via the `Mailer` so any
    /// actor with `ctx.mailer()` reaches the same registry — mirroring
    /// how [`Self::registry`] surfaces the routing table.
    pub fn capability_registry(&self) -> &Arc<CapabilityRegistry> {
        &self.capability_registry
    }

    /// Hand `mail` to the substrate for dispatch. `Inbox`-bound
    /// mailboxes run their handler on the caller thread; dropped /
    /// unknown recipients warn-and-discard (or bubble up to the
    /// hub-substrate when a `HubOutbound` is connected, per ADR-0037).
    pub fn push(&self, mail: Mail) {
        route_mail(
            mail,
            &self.registry,
            self.outbound.as_ref(),
            &self.handle_store,
            self.chassis_router.get().map(|b| &**b),
            &self.trace_handle,
        );
    }

    /// Publish a resolved handle and re-route every mail that was
    /// parked on it. Each parked mail re-walks against its kind
    /// schema; if the same payload still references a *different*
    /// missing handle, the re-walk parks it on that id, otherwise
    /// dispatch proceeds normally with the spliced-inline payload.
    ///
    /// Used by future host-fn shims (PR 3) and by chassis-level code
    /// that resolves handles synchronously. Returns the `PutError`
    /// from the underlying store on byte-budget / kind-id conflicts;
    /// in those cases parked mail stays parked and the caller decides
    /// how to recover.
    ///
    pub fn resolve_handle(
        &self,
        handle: HandleId,
        kind: KindId,
        bytes: Vec<u8>,
    ) -> Result<(), PutError> {
        self.handle_store.put(handle, kind, bytes)?;
        let parked = self.handle_store.take_parked(handle);
        let outbound = self.outbound.as_ref();
        let chassis_router = self.chassis_router.get().map(|b| &**b);
        for mail in parked {
            route_mail(
                mail,
                &self.registry,
                outbound,
                &self.handle_store,
                chassis_router,
                &self.trace_handle,
            );
        }
        Ok(())
    }

    /// Route a chassis-bound mailbox's `*Result` reply to `sender`
    /// with a single encode. `Session` / `EngineMailbox` hand off to
    /// the hub outbound (unchanged hub-wire format); `Component`
    /// pushes a fresh `Mail` into the target component's inbox so the
    /// guest's normal dispatch path delivers the reply. `None` is a
    /// silent drop — nobody asked for a reply.
    ///
    /// The reply mail carries `reply_to = None`: the receiver isn't
    /// expected to reply to a reply, and decorating with the
    /// closure-bound mailbox's id would produce a
    /// `ReplyEntry::Component` pointing at an entry that can't itself
    /// receive mail.
    pub fn send_reply<K>(&self, sender: ReplyTo, result: &K) -> bool
    where
        K: aether_data::Kind + serde::Serialize,
    {
        match sender.target {
            ReplyTarget::None => false,
            ReplyTarget::Session(_) | ReplyTarget::EngineMailbox { .. } => self
                .outbound
                .as_ref()
                .is_some_and(|outbound| outbound.send_reply(sender, result)),
            ReplyTarget::Component(mailbox) => {
                let payload = match postcard::to_allocvec(result) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(
                            target: "aether_substrate::mailer",
                            kind = K::NAME,
                            error = %e,
                            "reply encode failed",
                        );
                        return false;
                    }
                };
                // ADR-0042: echo the caller's correlation_id onto the
                // reply envelope so a `wait_reply_p32` parked on this
                // correlation picks the right reply out of the mpsc.
                // Reply target is None — nobody replies to a reply.
                let reply_to = ReplyTo::with_correlation(ReplyTarget::None, sender.correlation_id);
                self.push(Mail::new(mailbox, K::ID, payload, 1).with_reply_to(reply_to));
                true
            }
        }
    }
}

/// Resolve `mail.recipient` against the registry and dispatch
/// inline. `Inbox`-bound mailboxes forward to an actor's mpsc on the caller
/// thread (or fan out via the cap's mpsc, depending on the closure).
/// Dropped / unknown recipients warn-log and drop the mail.
///
/// Mail with a wired `HandleStore` walks through the ADR-0045
/// ref-resolver before recipient dispatch. Schemas with no `Ref`
/// nodes hit the no-op fast path; refs with all handles present
/// splice inline-form bytes into a fresh payload; mail that hits a
/// missing handle parks in the store and returns immediately
/// without dispatch.
// Routing pipeline runs as one function: chassis-mail switch,
// ref-resolver walk + dispatch, registry lookup, outbound forward.
// Splitting the steps would scatter the per-mail Vec<u8> buffer reuse
// and lose the linear "where does this envelope go?" read.
#[allow(clippy::too_many_lines)]
fn route_mail(
    mut mail: Mail,
    registry: &Registry,
    outbound: Option<&Arc<HubOutbound>>,
    store: &Arc<HandleStore>,
    chassis_router: Option<&(dyn Fn(Mail) + Send + Sync)>,
    trace_handle: &TraceHandle,
) {
    // ADR-0080 §5 chassis-mail switch — routed ahead of the registry
    // lookup so mail to `CHASSIS_MAILBOX_ID` reaches the chassis-
    // installed router without bubbling up as `UnresolvedMail`. Today
    // the router decodes `Settled { root }` and signals the
    // `SettlementRegistry`; future chassis-internal kinds add
    // matching arms inside the router closure.
    if mail.recipient == aether_data::MailboxId::CHASSIS_MAILBOX_ID {
        // ADR-0080 §2 producer hook: balance the `Sent` so settlement
        // chains drain (issue 838). Today the only chassis-addressed
        // kind is `Settled` itself, which is pushed bare without
        // lineage by `TraceObserverCapability::fire_settled`, so the
        // `MailId::NONE` short-circuit inside `record_finished`
        // no-ops. Stamped kinds (future debugger / describe_tree
        // replies) get the symmetric `Received`/`Finished` bracket.
        let inbound_mail_id = mail.mail_id;
        if let Some(router) = chassis_router {
            let thread_name = thread::current().name().map(str::to_owned);
            trace_handle.record_received(inbound_mail_id, thread_name);
            router(mail);
        } else {
            tracing::warn!(
                target: "aether_substrate::queue",
                kind = %mail.kind,
                "chassis-addressed mail dropped — no chassis router installed",
            );
        }
        trace_handle.record_finished(inbound_mail_id);
        return;
    }

    if let Some(descriptor) = registry.kind_descriptor(mail.kind)
        && handle_store::schema_contains_ref(&descriptor.schema)
    {
        match handle_store::walk_and_resolve(&descriptor.schema, &mail.payload, store) {
            Ok(WalkOutcome::Resolved { payload }) => {
                if let Cow::Owned(bytes) = payload {
                    mail.payload = bytes;
                }
                // Cow::Borrowed: mail.payload already matches the
                // resolved bytes (no substitutions happened).
            }
            Ok(WalkOutcome::Parked { handle, kind }) => {
                tracing::debug!(
                    target: "aether_substrate::handle_store",
                    handle = %handle,
                    kind = %kind,
                    recipient = ?mail.recipient,
                    "parking mail on missing handle",
                );
                store.park(handle, mail);
                return;
            }
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::handle_store",
                    kind = %mail.kind,
                    error = ?e,
                    recipient = ?mail.recipient,
                    "ref-walk failed against registered schema; mail dropped",
                );
                // ADR-0080 §2: balance the `Sent` so settlement chains
                // drain (issue 838). Parked mail (the `Ok(WalkOutcome::Parked)`
                // arm above) is deliberately NOT finished — it's held
                // for replay when the handle resolves.
                trace_handle.record_finished(mail.mail_id);
                return;
            }
        }
    }

    let recipient = mail.recipient;
    let inbound_mail_id = mail.mail_id;
    match registry.entry(recipient) {
        Some(MailboxEntry::Inbox(handler)) => {
            let kind_name = registry.kind_name(mail.kind).unwrap_or_default();
            // Mail reaching a closure-bound mailbox through `push`
            // came from substrate core or a chassis (e.g. the frame
            // loop's FrameStats push, platform input fan-out). Per
            // ADR-0011 origin is `None`. Components reach
            // closure-bound mailboxes via `ComponentCtx::send` inline
            // and never enter `push`.
            //
            // ADR-0080 §2 producer-hook note (issue 838): no
            // `Received`/`Finished` bracket fires here. `Inbox`
            // is the actor-enqueue variant — the handler body
            // pushes the envelope onto an mpsc inbox, and the
            // actor's dispatch loop at `actor/native/dispatch.rs`
            // records the bracket downstream when its worker picks
            // the envelope up. Adding a bracket here would
            // double-count `Finished` and fire settlement
            // prematurely (surfaced by
            // `aether-substrate-bundle::rpc_engine_routing` as
            // ReplyEnd before ReplyEvent). Synchronous handlers live
            // on the [`MailboxEntry::Inline`] arm below — they get
            // the bracket because nothing downstream owns it.
            //
            // iamacoffeepot/aether#848: payload + kind_name move
            // into [`OwnedDispatch`] rather than being borrowed.
            // The handler is `Arc<dyn InboxHandler>` whose `enqueue`
            // takes owned bytes — cap closures move directly into
            // their downstream envelope (via
            // `Envelope::from(OwnedDispatch)`) with zero payload
            // copies.
            handler.enqueue(OwnedDispatch {
                kind: mail.kind,
                kind_name,
                origin: None,
                sender: mail.reply_to,
                payload: mail.payload,
                count: mail.count,
                mail_id: mail.mail_id,
                root: mail.root,
                parent_mail: mail.parent_mail,
            });
        }
        Some(MailboxEntry::Inline(handler)) => {
            let kind_name = registry.kind_name(mail.kind).unwrap_or_default();
            // ADR-0080 §2 producer hook: synchronous handler.
            // Bracket the inline call with `Received`/`Finished`
            // so the chain's `in_flight` balances and settlement
            // subscribers wake (issue 838). Distinct from `Inbox`
            // above — see that arm's doc for the
            // double-count-prematurely-settle hazard the split
            // avoids.
            let thread_name = thread::current().name().map(str::to_owned);
            trace_handle.record_received(inbound_mail_id, thread_name);
            handler.dispatch(MailDispatch {
                kind: mail.kind,
                kind_name: &kind_name,
                origin: None,
                sender: mail.reply_to,
                payload: &mail.payload,
                count: mail.count,
                mail_id: mail.mail_id,
                root: mail.root,
                parent_mail: mail.parent_mail,
            });
            trace_handle.record_finished(inbound_mail_id);
        }
        Some(MailboxEntry::Dropped) => {
            tracing::warn!(
                target: "aether_substrate::queue",
                mailbox = %recipient,
                "mail to dropped mailbox — discarded",
            );
            // ADR-0080 §2: balance the `Sent` so settlement chains
            // drain (issue 838). No `Received` — no handler ran.
            trace_handle.record_finished(inbound_mail_id);
        }
        None => {
            // ADR-0037 Phase 1: unknown-locally mailboxes bubble up
            // to the hub-substrate when a live outbound is wired.
            // The hub resolves the id against its own registry and
            // dispatches; if it doesn't know the id either, it
            // warns on its side (end-of-line). Fall back to the
            // local warn-drop when no hub is attached (single-host
            // dev, or the hub chassis itself).
            if let Some(outbound) = outbound
                && outbound.is_connected()
            {
                // ADR-0037 Phase 2: carry the local sending
                // component's mailbox id so the hub can build a
                // `ReplyTo::EngineMailbox { engine_id, mailbox_id }`
                // for the receiving component. `None` for mail
                // with no local component origin (substrate-generated).
                // Recovered from
                // `reply_to.target = Component(_)` set by
                // `ComponentCtx::send` / `NativeBinding::send_mail`
                // (issue #644).
                let source_mailbox_id = match mail.reply_to.target {
                    ReplyTarget::Component(id) => Some(id),
                    _ => None,
                };
                // ADR-0042: carry the correlation through the bubble-
                // up frame so a reply coming back via Phase-2 reply
                // routing lands at the originator's `wait_reply_p32`.
                let correlation_id = mail.reply_to.correlation_id;
                outbound.egress_unresolved_mail(
                    recipient,
                    mail.kind,
                    mail.payload,
                    mail.count,
                    source_mailbox_id,
                    correlation_id,
                );
                // ADR-0080 §2: per-engine settlement (issue 838) —
                // the local engine treats egress-to-hub as "Finished
                // from our perspective." The hub processes the
                // bubbled-up mail on its own settlement domain; no
                // wire signal exists today for federated cross-engine
                // settlement (and the issue body parks that design).
                trace_handle.record_finished(inbound_mail_id);
                return;
            }
            // Issue 963: `aether.log.tail` (`actor_logs`) to an
            // unresolved mailbox synthesizes a `LogTailResult::Err`
            // reply instead of silently warn-dropping, so the MCP
            // caller's `call_one` sees one `ReplyEvent` (a clean "that
            // mailbox doesn't exist" signal) rather than `got 0
            // replies`. Narrow per-kind treatment (Option B) — the
            // general `aether.mail.unresolved` reply for every
            // reply-expecting kind is parked (Option C, lib.rs:86).
            // Only the `Component` reply target (the path the MCP Call
            // takes via `RpcServerCapability::handle_call`) is routed;
            // `Session`/`EngineMailbox` targets fall through to the
            // warn-drop, keeping the blast radius minimal.
            if mail.kind.0 == <aether_kinds::LogTail as aether_data::Kind>::ID.0 {
                let err = aether_kinds::LogTailResult::Err {
                    error: format!("mailbox {recipient} not registered on engine"),
                };
                if let Ok(payload) = postcard::to_allocvec(&err)
                    && let ReplyTarget::Component(target) = mail.reply_to.target
                {
                    let reply_to =
                        ReplyTo::with_correlation(ReplyTarget::None, mail.reply_to.correlation_id);
                    route_mail(
                        Mail::new(
                            target,
                            <aether_kinds::LogTailResult as aether_data::Kind>::ID,
                            payload,
                            1,
                        )
                        .with_reply_to(reply_to),
                        registry,
                        outbound,
                        store,
                        chassis_router,
                        trace_handle,
                    );
                }
                // The synthesized reply is a fresh un-lineaged mail
                // (`MailId::NONE`); the inbound still records `Finished`
                // so its settlement chain balances (issue 838).
                trace_handle.record_finished(inbound_mail_id);
                return;
            }
            tracing::warn!(
                target: "aether_substrate::queue",
                mailbox = %recipient,
                "mail to unknown mailbox — dropped",
            );
            // ADR-0080 §2: balance the `Sent` so settlement chains
            // drain (issue 838). Sokoban's `on_tick` sends to an
            // unloaded `"camera"` mailbox every tick; without this
            // every Tick chain has an orphaned `Sent` and never
            // settles.
            trace_handle.record_finished(inbound_mail_id);
        }
    }
}

#[cfg(test)]
// Mailer integration tests stage senders / receivers / multi-step
// dispatch fixtures inline so the round-trip read top-to-bottom;
// extracting helpers would split the path-under-test across files.
#[allow(clippy::too_many_lines)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests {
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::handle_store::HandleStore;
    use crate::mail::MailboxId;
    use crate::mail::outbound::EgressEvent;
    use crate::mail::registry::{InboxHandler, InlineHandler};
    use aether_data::{Kind, Ref};
    use aether_data::{KindDescriptor, NamedField, Primitive, SchemaCell, SchemaType};

    /// ADR-0037 Phase 1: a live outbound + unknown mailbox id
    /// forwards `MailToHubSubstrate` upstream instead of
    /// warn-dropping. The forwarded frame carries the exact
    /// mailbox id / kind / payload / count the caller pushed.
    #[test]
    fn unknown_mailbox_with_connected_outbound_bubbles_up() {
        let (outbound, outbound_rx) = HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(64 * 1024));

        let mailer = Mailer::new(Arc::clone(&registry), store).with_outbound(Arc::clone(&outbound));

        let unknown = MailboxId(0xDEAD_BEEF_u64);
        let kind = KindId(0xABCD_u64);
        let payload = vec![1, 2, 3];
        mailer.push(Mail::new(unknown, kind, payload.clone(), 1));

        let event = outbound_rx.try_recv().expect("bubble-up event emitted");
        match event {
            EgressEvent::UnresolvedMail {
                recipient_mailbox_id,
                kind_id,
                payload: p,
                count,
                ..
            } => {
                assert_eq!(recipient_mailbox_id, unknown);
                assert_eq!(kind_id, kind);
                assert_eq!(p, payload);
                assert_eq!(count, 1);
            }
            other => panic!("expected UnresolvedMail, got {other:?}"),
        }
    }

    /// No outbound wired (or disconnected): unknown mailbox stays
    /// warn-drop. Asserts by showing the outbound channel stays
    /// empty even though we pushed — the warn-drop path doesn't
    /// generate a frame.
    #[test]
    fn unknown_mailbox_without_outbound_warn_drops() {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(64 * 1024));

        let mailer = Mailer::new(Arc::clone(&registry), store);
        // Deliberately no `with_outbound` — exercises the local
        // warn-drop path (the hub chassis path).

        let unknown = MailboxId(0xDEAD_BEEF_u64);
        mailer.push(Mail::new(unknown, KindId(0xABCD), vec![], 0));
        // No panic is the test; the warn path logs and returns.
    }

    /// Issue 963: recorder capture row — `(kind, correlation, payload)`
    /// for each reply a stand-in RPC-server mailbox receives. Aliased
    /// to keep the `Arc<RwLock<Vec<...>>>` off the `type_complexity`
    /// lint at the two recorder sites below.
    type RecordedReplies = Arc<RwLock<Vec<(KindId, u64, Vec<u8>)>>>;

    /// Register an inline mailbox that records each reply's kind,
    /// correlation, and payload — a stand-in for the RPC-server reply
    /// target the MCP Call's `Component` reply hop lands at. Returns
    /// the recorder's `MailboxId` plus the shared capture buffer.
    fn record_inline(registry: &Registry) -> (MailboxId, RecordedReplies) {
        let recorded: RecordedReplies = Arc::new(RwLock::new(Vec::new()));
        let recorded_for_handler = Arc::clone(&recorded);
        let recorder_id = registry.register_inline(
            "test.rpc_server_reply",
            Arc::new(move |dispatch: MailDispatch<'_>| {
                recorded_for_handler.write().unwrap().push((
                    dispatch.kind,
                    dispatch.sender.correlation_id,
                    dispatch.payload.to_vec(),
                ));
            }),
        );
        (recorder_id, recorded)
    }

    /// Issue 963: `aether.log.tail` to an unregistered mailbox (no
    /// outbound) synthesizes a `LogTailResult::Err` reply routed back
    /// to the inbound's `Component` reply target instead of warn-
    /// dropping. Stands in the RPC-server reply mailbox with a
    /// recording inline handler; asserts exactly one reply of kind
    /// `LogTailResult::ID`, decoding to `Err { error }` naming the
    /// recipient id, with the correlation echoed.
    #[test]
    fn unknown_mailbox_log_tail_synthesizes_err_reply() {
        use aether_kinds::{LogTail, LogTailResult};

        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(64 * 1024));
        let mailer = Mailer::new(Arc::clone(&registry), store);

        let (recorder_id, recorded) = record_inline(&registry);

        let unknown = MailboxId(0xDEAD_BEEF_u64);
        mailer.push(
            Mail::new(unknown, <LogTail as Kind>::ID, vec![], 1).with_reply_to(
                ReplyTo::with_correlation(ReplyTarget::Component(recorder_id), 0xCAFE),
            ),
        );

        let recorded = recorded.read().unwrap();
        assert_eq!(recorded.len(), 1, "exactly one synthesized reply");
        let (kind, correlation, payload) = &recorded[0];
        assert_eq!(*kind, <LogTailResult as Kind>::ID);
        assert_eq!(*correlation, 0xCAFE, "correlation echoed onto the reply");
        match postcard::from_bytes::<LogTailResult>(payload).unwrap() {
            LogTailResult::Err { error } => assert!(
                error.contains(&unknown.to_string()),
                "error names the recipient id: {error}",
            ),
            other @ LogTailResult::Ok { .. } => {
                panic!("expected LogTailResult::Err, got {other:?}")
            }
        }
    }

    /// Issue 963: the synthesized-Err branch is narrow — a non-
    /// `LogTail` kind to an unregistered mailbox still warn-drops with
    /// no reply, even with a live `Component` reply target. Pins the
    /// scope so the change doesn't start replying for every kind
    /// (Option B, not Option C).
    #[test]
    fn unknown_mailbox_non_log_tail_still_warn_drops() {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(64 * 1024));
        let mailer = Mailer::new(Arc::clone(&registry), store);

        let (recorder_id, recorded) = record_inline(&registry);

        let unknown = MailboxId(0xDEAD_BEEF_u64);
        // Arbitrary non-`LogTail` kind id — the reply branch must not fire.
        mailer.push(Mail::new(unknown, KindId(0xABCD), vec![], 1).with_reply_to(
            ReplyTo::with_correlation(ReplyTarget::Component(recorder_id), 0xCAFE),
        ));

        assert!(
            recorded.read().unwrap().is_empty(),
            "non-LogTail unknown-mailbox mail warn-drops with no reply",
        );
    }

    // ------------------------------------------------------------
    // ADR-0045 Ref-resolution integration
    // ------------------------------------------------------------

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug, Clone)]
    struct Note {
        body: String,
        seq: u32,
    }
    impl Kind for Note {
        const NAME: &'static str = "test.mailer_note";
        // Stable test sentinel — distinct from real schema-hashed kind ids.
        const ID: KindId = KindId(0xDEAD_BEEF_0003_0001);
    }

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug, Clone)]
    struct HeldNote {
        held: Ref<Note>,
        seq: u32,
    }
    impl Kind for HeldNote {
        const NAME: &'static str = "test.mailer_held_note";
        // Stable test sentinel — distinct from real schema-hashed kind ids.
        const ID: KindId = KindId(0xDEAD_BEEF_0003_0002);
    }

    fn note_schema() -> SchemaType {
        SchemaType::Struct {
            fields: Cow::Owned(vec![
                NamedField {
                    name: Cow::Borrowed("body"),
                    ty: SchemaType::String,
                },
                NamedField {
                    name: Cow::Borrowed("seq"),
                    ty: SchemaType::Scalar(Primitive::U32),
                },
            ]),
            repr_c: false,
        }
    }

    fn held_note_schema() -> SchemaType {
        SchemaType::Struct {
            fields: Cow::Owned(vec![
                NamedField {
                    name: Cow::Borrowed("held"),
                    ty: SchemaType::Ref(SchemaCell::owned(note_schema())),
                },
                NamedField {
                    name: Cow::Borrowed("seq"),
                    ty: SchemaType::Scalar(Primitive::U32),
                },
            ]),
            repr_c: false,
        }
    }

    /// Capture-bytes sink: records every payload it receives so a
    /// test can assert what bytes the dispatcher delivered.
    struct CapturingSink {
        captured: Arc<RwLock<Vec<Vec<u8>>>>,
        delivery_count: Arc<AtomicUsize>,
    }
    impl CapturingSink {
        fn new() -> Self {
            Self {
                captured: Arc::new(RwLock::new(Vec::new())),
                delivery_count: Arc::new(AtomicUsize::new(0)),
            }
        }
        /// Inline-variant handler: synchronous capture body. Used by
        /// the `register_inline` test sites; mailer brackets the
        /// call so settlement balances.
        fn inline_handler(&self) -> Arc<dyn InlineHandler> {
            let captured = Arc::clone(&self.captured);
            let count = Arc::clone(&self.delivery_count);
            Arc::new(move |dispatch: MailDispatch<'_>| {
                captured.write().unwrap().push(dispatch.payload.to_vec());
                count.fetch_add(1, Ordering::SeqCst);
            })
        }

        /// Inbox-variant handler: receives [`OwnedDispatch`] and
        /// moves the payload into the captured Vec (no `to_vec()`
        /// clone). Used by `register_inbox` test sites that exercise
        /// the actor-enqueue dispatch path. Bracket owned downstream
        /// — these tests don't subscribe to settlement so the
        /// "downstream never finishes" path is intentional and
        /// harmless.
        fn inbox_handler(&self) -> Arc<dyn InboxHandler> {
            let captured = Arc::clone(&self.captured);
            let count = Arc::clone(&self.delivery_count);
            Arc::new(move |dispatch: OwnedDispatch| {
                captured.write().unwrap().push(dispatch.payload);
                count.fetch_add(1, Ordering::SeqCst);
            })
        }
    }

    fn make_mailer() -> (Arc<Registry>, Arc<Mailer>, Arc<HandleStore>) {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(64 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), Arc::clone(&store)));
        (registry, mailer, store)
    }

    /// Mail to a sink whose kind has no `Ref` fields takes the
    /// fast path: no walker invocation, payload delivered verbatim.
    #[test]
    fn ref_free_kind_passes_through_mailer() {
        let (registry, mailer, _store) = make_mailer();
        let note_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: Note::NAME.into(),
                schema: note_schema(),
            })
            .unwrap();
        let sink = CapturingSink::new();
        let sink_id = registry.register_inbox("test.sink", sink.inbox_handler());

        let note = Note {
            body: "verbatim".into(),
            seq: 1,
        };
        let bytes = postcard::to_allocvec(&note).unwrap();
        mailer.push(Mail::new(sink_id, note_id, bytes.clone(), 1));

        let captured = sink.captured.read().unwrap().clone();
        assert_eq!(captured, vec![bytes]);
    }

    /// Mail with a `Handle` ref whose handle is missing parks in the
    /// store. The sink does not see it. After `resolve_handle` lands
    /// the entry, the parked mail re-routes and the sink receives
    /// the spliced payload.
    #[test]
    fn handle_ref_parks_then_resolves_through_mailer() {
        let (registry, mailer, store) = make_mailer();
        let outer_kind_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: HeldNote::NAME.into(),
                schema: held_note_schema(),
            })
            .unwrap();
        let inner_kind_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: Note::NAME.into(),
                schema: note_schema(),
            })
            .unwrap();

        let sink = CapturingSink::new();
        let sink_id = registry.register_inbox("test.sink", sink.inbox_handler());

        // Push HeldNote mail with `held = Handle(7)`. Handle 7 is
        // not in the store yet — the mail must park. We construct
        // `Ref::Handle` directly with the registry-derived
        // `inner_kind_id` so the walker's debug-assert (stored
        // kind_id == wire kind_id) holds when the resolve fires.
        let outer = HeldNote {
            held: Ref::Handle {
                id: 7,
                kind_id: inner_kind_id.0,
            },
            seq: 11,
        };
        let outer_bytes = postcard::to_allocvec(&outer).unwrap();
        mailer.push(Mail::new(sink_id, outer_kind_id, outer_bytes, 1));
        assert_eq!(
            sink.delivery_count.load(Ordering::SeqCst),
            0,
            "mail must not dispatch until handle resolves",
        );
        assert_eq!(store.parked_count(HandleId(7)), 1);

        // Resolve handle 7. The mail should now flow to the sink
        // with the inner Note bytes spliced inline.
        let inner = Note {
            body: "resolved".into(),
            seq: 99,
        };
        let inner_bytes = postcard::to_allocvec(&inner).unwrap();
        mailer
            .resolve_handle(HandleId(7), inner_kind_id, inner_bytes)
            .unwrap();
        assert_eq!(store.parked_count(HandleId(7)), 0);
        assert_eq!(sink.delivery_count.load(Ordering::SeqCst), 1);

        let captured = sink.captured.read().unwrap();
        let delivered: HeldNote = postcard::from_bytes(&captured[0]).unwrap();
        assert_eq!(delivered.seq, 11);
        match delivered.held {
            Ref::Inline(got) => {
                assert_eq!(got.body, "resolved");
                assert_eq!(got.seq, 99);
            }
            Ref::Handle { .. } => panic!("walker must replace Handle with Inline"),
        }
    }

    /// Mail whose payload is malformed against the registered
    /// schema (e.g. truncated bytes) gets dropped with a warn log,
    /// not delivered to the sink. Pin the contract — without this
    /// guard the sink would receive bytes that don't decode against
    /// the schema it expects.
    #[test]
    fn malformed_ref_payload_drops_mail() {
        let (registry, mailer, _store) = make_mailer();
        let kind_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: HeldNote::NAME.into(),
                schema: held_note_schema(),
            })
            .unwrap();

        let sink = CapturingSink::new();
        let sink_id = registry.register_inbox("test.sink", sink.inbox_handler());

        // Truncated payload — the walker bails Truncated mid-walk.
        mailer.push(Mail::new(sink_id, kind_id, vec![0u8; 1], 1));
        assert_eq!(sink.delivery_count.load(Ordering::SeqCst), 0);
    }

    /// Issue 838: ADR-0080 §2 producer-hook coverage on the inline
    /// dispatch arms. Each test below stamps a `Mail` with a unique
    /// sender mailbox id, pushes through the Mailer, and drains the
    /// process-global trace queue filtering on its own sender so
    /// events from concurrent tests in the same binary don't
    /// confuse the assertion.
    use aether_data::MailId;
    use aether_kinds::trace::TraceEvent;
    use crossbeam_queue::SegQueue;

    /// Borrow the queue Arc from a `Mailer`'s default trace handle.
    /// `Mailer::new` allocates a fresh handle per construction
    /// (iamacoffeepot/aether#953 retired the process-global), so each
    /// test's queue is naturally isolated. The `drain_events_for`
    /// filter by `sender` is no longer strictly necessary but kept
    /// to match the original assertion shape.
    fn install_test_trace_handle(mailer: &Mailer) -> Arc<SegQueue<TraceEvent>> {
        Arc::clone(mailer.trace_handle().queue())
    }

    /// Drain the trace queue and return only events whose `mail_id`
    /// is keyed to `sender` — lets parallel tests share the global
    /// queue without false positives.
    fn drain_events_for(queue: &SegQueue<TraceEvent>, sender: MailboxId) -> Vec<TraceEvent> {
        let mut out = Vec::new();
        let mut leftover = Vec::new();
        while let Some(event) = queue.pop() {
            let belongs = match &event {
                TraceEvent::Sent { mail_id, .. }
                | TraceEvent::Received { mail_id, .. }
                | TraceEvent::Finished { mail_id, .. } => mail_id.sender == sender,
                TraceEvent::HoldOpen { root, .. } | TraceEvent::Release { root, .. } => {
                    root.sender == sender
                }
            };
            if belongs {
                out.push(event);
            } else {
                leftover.push(event);
            }
        }
        for ev in leftover {
            queue.push(ev);
        }
        out
    }

    /// Unknown-mailbox warn-drop (no outbound wired): the chain's
    /// `Sent` is still balanced by `Finished` so settlement
    /// subscribers don't hang.
    #[test]
    fn unknown_mailbox_warn_drop_records_finished() {
        let sender = MailboxId(0x8380_0002_0000_0000);
        let inbound_mail_id = MailId::new(sender, 1);

        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(64 * 1024));
        let mailer = Mailer::new(Arc::clone(&registry), store);
        let queue = install_test_trace_handle(&mailer);

        let unknown = MailboxId(0xDEAD_BEEF_BABE);
        let mail = Mail::new(unknown, KindId(0xABCD), vec![], 1).with_lineage(
            inbound_mail_id,
            inbound_mail_id,
            None,
        );
        mailer.push(mail);

        let events = drain_events_for(&queue, sender);
        let finished = events.iter().any(
            |e| matches!(e, TraceEvent::Finished { mail_id, .. } if *mail_id == inbound_mail_id),
        );
        assert!(finished, "expected Finished for warn-drop; got {events:?}");
    }

    /// Egress-to-hub: per-engine settlement (issue 838) — the local
    /// engine treats the egress as "Finished from our perspective"
    /// so local subscribers don't wait on a hub roundtrip that
    /// doesn't exist on the wire today.
    #[test]
    fn unknown_mailbox_egress_records_finished_locally() {
        let sender = MailboxId(0x8380_0003_0000_0000);
        let inbound_mail_id = MailId::new(sender, 1);

        let (outbound, outbound_rx) = HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(64 * 1024));
        let mailer = Mailer::new(Arc::clone(&registry), store).with_outbound(Arc::clone(&outbound));
        let queue = install_test_trace_handle(&mailer);

        let unknown = MailboxId(0xDEAD_BEEF_F00D);
        let mail = Mail::new(unknown, KindId(0xABCD), vec![9, 9], 1).with_lineage(
            inbound_mail_id,
            inbound_mail_id,
            None,
        );
        mailer.push(mail);

        // Sanity: the bubble-up actually happened.
        let _ = outbound_rx.try_recv().expect("bubble-up event emitted");

        let events = drain_events_for(&queue, sender);
        let finished = events.iter().any(
            |e| matches!(e, TraceEvent::Finished { mail_id, .. } if *mail_id == inbound_mail_id),
        );
        assert!(
            finished,
            "expected Finished after egress (per-engine settlement); got {events:?}"
        );
    }

    /// Issue 838 diff 2: synchronous dispatch via the `Inline`
    /// arm runs the handler inline AND emits a `Received`/`Finished`
    /// bracket so the chain's `in_flight` balances and settlement
    /// subscribers wake. Mirrors what the actor-dispatch loop does
    /// for `Inbox` recipients, but on the pushing thread.
    #[test]
    fn inline_arm_brackets_handler_with_received_and_finished() {
        let sender = MailboxId(0x8380_0004_0000_0000);
        let inbound_mail_id = MailId::new(sender, 1);

        let (registry, mailer, _store) = make_mailer();
        let queue = install_test_trace_handle(&mailer);
        let sink = CapturingSink::new();
        let sink_id = registry.register_inline("test.838.sink", sink.inline_handler());

        let mail = Mail::new(sink_id, KindId(0xCAFE_BABE), vec![1, 2, 3], 1).with_lineage(
            inbound_mail_id,
            inbound_mail_id,
            None,
        );
        mailer.push(mail);

        assert_eq!(
            sink.delivery_count.load(Ordering::SeqCst),
            1,
            "sink handler should have run"
        );

        let events = drain_events_for(&queue, sender);
        let received = events.iter().any(
            |e| matches!(e, TraceEvent::Received { mail_id, .. } if *mail_id == inbound_mail_id),
        );
        let finished = events.iter().any(
            |e| matches!(e, TraceEvent::Finished { mail_id, .. } if *mail_id == inbound_mail_id),
        );
        assert!(
            received,
            "expected Received for sink dispatch; got {events:?}"
        );
        assert!(
            finished,
            "expected Finished for sink dispatch; got {events:?}"
        );
    }

    /// Issue 838 diff 2 regression guard: actor-enqueue dispatch
    /// via `Inbox` MUST NOT emit Received/Finished from the Mailer
    /// side. The actor's dispatch loop at
    /// `actor/native/dispatch.rs:85` owns the bracket; doubling it
    /// here fires settlement prematurely and breaks
    /// `aether-substrate-bundle::rpc_engine_routing` (`ReplyEnd`
    /// before `ReplyEvent`). This test pins the contract so a
    /// future "let's add the bracket for symmetry" refactor fails
    /// loudly.
    #[test]
    fn inbox_arm_does_not_bracket_in_mailer() {
        let sender = MailboxId(0x8380_0005_0000_0000);
        let inbound_mail_id = MailId::new(sender, 1);

        let (registry, mailer, _store) = make_mailer();
        let queue = install_test_trace_handle(&mailer);
        let sink = CapturingSink::new();
        // Register as `register_inbox` (the actor-enqueue
        // contract), NOT `register_inline`.
        let recipient =
            registry.register_inbox("test.838.closure_regression", sink.inbox_handler());

        let mail = Mail::new(recipient, KindId(0xCAFE_BABE), vec![4, 5, 6], 1).with_lineage(
            inbound_mail_id,
            inbound_mail_id,
            None,
        );
        mailer.push(mail);

        // Handler still runs — that's how mail reaches the actor's
        // mpsc inbox in production.
        assert_eq!(sink.delivery_count.load(Ordering::SeqCst), 1);

        let events = drain_events_for(&queue, sender);
        let received_from_mailer = events.iter().any(
            |e| matches!(e, TraceEvent::Received { mail_id, .. } if *mail_id == inbound_mail_id),
        );
        let finished_from_mailer = events.iter().any(
            |e| matches!(e, TraceEvent::Finished { mail_id, .. } if *mail_id == inbound_mail_id),
        );
        assert!(
            !received_from_mailer,
            "Inbox arm must not emit Received from Mailer — actor dispatch loop owns it (issue 838 hazard). Got: {events:?}"
        );
        assert!(
            !finished_from_mailer,
            "Inbox arm must not emit Finished from Mailer — actor dispatch loop owns it (issue 838 hazard). Got: {events:?}"
        );
    }

    /// Issue 838 diff 2: mail that parks on a missing handle does
    /// NOT emit Finished. The chain stays elevated until the
    /// handle is published and the mail is replayed through
    /// `Mailer::push`, at which point the now-resolved walk
    /// reaches a terminal arm (Sink here) and that arm's bracket
    /// fires. Pins "Parked is not Finished" — semantically
    /// distinct from Ref-walk Err (which IS terminal and fires
    /// Finished, covered by the existing
    /// `malformed_ref_payload_drops_mail` plus the issue 839 Ref-
    /// walk-Err Finished record).
    #[test]
    fn ref_walk_parked_defers_finished_until_handle_publish() {
        use aether_data::HandleId;

        let sender = MailboxId(0x8380_0006_0000_0000);
        let inbound_mail_id = MailId::new(sender, 1);

        let (registry, mailer, store) = make_mailer();
        let queue = install_test_trace_handle(&mailer);
        // Register both kinds so the mailer walks the outer schema
        // and the handle store recognises the inner one. The
        // registry-derived kind id is what `Ref::Handle.kind_id`
        // must carry (the walker's debug-assert compares stored
        // kind_id == wire kind_id) — same pattern as the existing
        // `handle_ref_parks_then_resolves_through_mailer` test.
        let outer_kind_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: HeldNote::NAME.into(),
                schema: held_note_schema(),
            })
            .unwrap();
        let inner_kind_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: Note::NAME.into(),
                schema: note_schema(),
            })
            .unwrap();
        let sink = CapturingSink::new();
        let sink_id = registry.register_inline("test.838.park_defer", sink.inline_handler());

        // Build a payload that references a handle not yet in the
        // store — the walker returns `Parked`, mail held.
        let handle = HandleId(0x1234_5678_9ABC_DEF0);
        let held: HeldNote = HeldNote {
            held: Ref::Handle {
                id: handle.0,
                kind_id: inner_kind_id.0,
            },
            seq: 7,
        };
        let payload = postcard::to_allocvec(&held).unwrap();

        let mail = Mail::new(sink_id, outer_kind_id, payload, 1).with_lineage(
            inbound_mail_id,
            inbound_mail_id,
            None,
        );
        mailer.push(mail);

        // Handler hasn't run; mail is parked. NO Finished yet.
        assert_eq!(
            sink.delivery_count.load(Ordering::SeqCst),
            0,
            "parked mail must not dispatch"
        );
        assert_eq!(
            store.parked_count(handle),
            1,
            "mail should be parked under the missing handle"
        );
        let events_before_publish = drain_events_for(&queue, sender);
        let finished_before = events_before_publish.iter().any(
            |e| matches!(e, TraceEvent::Finished { mail_id, .. } if *mail_id == inbound_mail_id),
        );
        assert!(
            !finished_before,
            "Parked mail must NOT emit Finished — the chain stays elevated until publish (issue 838). Got: {events_before_publish:?}"
        );

        // Publish the handle with matching `Note` bytes; resolve
        // and replay through the mailer. The walker reruns,
        // resolves the ref, and the sink's bracket fires.
        let note = Note {
            body: "hi".into(),
            seq: 99,
        };
        let note_bytes = postcard::to_allocvec(&note).unwrap();
        mailer
            .resolve_handle(handle, inner_kind_id, note_bytes)
            .expect("resolve_handle");

        assert_eq!(
            sink.delivery_count.load(Ordering::SeqCst),
            1,
            "after publish + replay, sink should have dispatched once"
        );

        let events_after = drain_events_for(&queue, sender);
        let finished_after = events_after.iter().any(
            |e| matches!(e, TraceEvent::Finished { mail_id, .. } if *mail_id == inbound_mail_id),
        );
        assert!(
            finished_after,
            "after publish, the resumed dispatch should have fired Finished. Got: {events_after:?}"
        );
    }

    /// Issue 838 diff 2: exhaustive meta-test asserting every
    /// `Mailer::push` short-circuit produces the correct ADR-0080
    /// §2 lifecycle events for stamped (non-NONE) mail. The
    /// `DispatchPath` enum mirrors the real dispatch surface; the
    /// match below is exhaustive, so a contributor adding a new
    /// path (a new `MailboxEntry` variant, a new `route_mail`
    /// short-circuit) MUST extend this test — that's the
    /// forcing function for lifecycle-hook coverage.
    ///
    /// Both production bugs we shipped on this work would have
    /// failed this test immediately:
    /// - Original iamacoffeepot/aether#838 leak: `Inline` case would have shown no
    ///   Finished, expected Bracket.
    /// - iamacoffeepot/aether#839-attempt-1 double-count: `Inbox` case would have shown
    ///   Finished from the Mailer side, expected `NeitherFromMailer`.
    #[test]
    fn every_mailer_push_path_produces_correct_lifecycle_events() {
        // Static link to `MailboxEntry`: a new variant added there
        // breaks this `match`, which fails to compile, which
        // forces the contributor to add a case to `DispatchPath`
        // and the test loop below. Comment is normative.
        fn dispatch_path_for_entry(entry: &MailboxEntry) -> &'static str {
            match entry {
                MailboxEntry::Inbox(_) => "Inbox",
                MailboxEntry::Inline(_) => "Inline",
                MailboxEntry::Dropped => "Dropped",
            }
        }

        enum Expect {
            /// Sync handler ran inline → Received + Finished.
            Bracket,
            /// Terminal arm (drop / warn / egress / ref-walk-err /
            /// router-missing) → Finished only.
            FinishedOnly,
            /// Held — mail deferred via `HandleStore::park` → neither.
            HeldNeither,
            /// Actor-enqueue `Inbox` → no bracket from Mailer side
            /// (downstream actor dispatch loop owns it).
            NeitherFromMailer,
        }

        struct Case {
            name: &'static str,
            expect: Expect,
            // Returns the stamped `MailId` plus the filtered trace
            // events for that case. Each case builds its own
            // `Mailer` whose default trace handle holds the queue
            // we drain at the end of the closure (per-mailer queues
            // post iamacoffeepot/aether#953 — no process-global).
            run: Box<dyn FnOnce() -> (MailId, Vec<TraceEvent>)>,
        }

        // Touch the helper so the compiler considers it live.
        let _ = dispatch_path_for_entry(&MailboxEntry::Dropped);

        // Each case: (case-name, expectation, push-fn that returns
        // the stamped mail_id + drained events). Cases construct
        // their own Mailer fixtures so chassis-router / outbound /
        // handle-store-Parked setups can vary independently.

        let cases: Vec<Case> = vec![
            // 1. Inline arm — bracket.
            Case {
                name: "Inline",
                expect: Expect::Bracket,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD01_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (registry, mailer, _store) = make_mailer();
                    let queue = install_test_trace_handle(&mailer);
                    let sink = CapturingSink::new();
                    let id = registry.register_inline("test.meta.sink", sink.inline_handler());
                    mailer.push(
                        Mail::new(id, KindId(0xFEED), vec![], 1)
                            .with_lineage(mail_id, mail_id, None),
                    );
                    (mail_id, drain_events_for(&queue, sender))
                }),
            },
            // 2. Inbox arm — no bracket from Mailer (regression
            // guard for actor-enqueue contract).
            Case {
                name: "Inbox (actor-enqueue)",
                expect: Expect::NeitherFromMailer,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD02_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (registry, mailer, _store) = make_mailer();
                    let queue = install_test_trace_handle(&mailer);
                    let sink = CapturingSink::new();
                    let id = registry.register_inbox("test.meta.closure", sink.inbox_handler());
                    mailer.push(
                        Mail::new(id, KindId(0xFEED), vec![], 1)
                            .with_lineage(mail_id, mail_id, None),
                    );
                    (mail_id, drain_events_for(&queue, sender))
                }),
            },
            // 3. Dropped arm — Finished only.
            Case {
                name: "Dropped",
                expect: Expect::FinishedOnly,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD03_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (registry, mailer, _store) = make_mailer();
                    let queue = install_test_trace_handle(&mailer);
                    let id = registry.register_inbox("test.meta.dropped", Arc::new(|_| {}));
                    let _ = registry.drop_mailbox(id).expect("drop");
                    mailer.push(
                        Mail::new(id, KindId(0xFEED), vec![], 1)
                            .with_lineage(mail_id, mail_id, None),
                    );
                    (mail_id, drain_events_for(&queue, sender))
                }),
            },
            // 4. None warn-drop (no outbound) — Finished only.
            Case {
                name: "None warn-drop (no outbound)",
                expect: Expect::FinishedOnly,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD04_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (_registry, mailer, _store) = make_mailer();
                    let queue = install_test_trace_handle(&mailer);
                    mailer.push(
                        Mail::new(MailboxId(0xDEAD_BEEF_0001), KindId(0xFEED), vec![], 1)
                            .with_lineage(mail_id, mail_id, None),
                    );
                    (mail_id, drain_events_for(&queue, sender))
                }),
            },
            // 5. None egress to hub — Finished only (per-engine
            // settlement; hub settlement is its own domain).
            Case {
                name: "None egress (outbound wired)",
                expect: Expect::FinishedOnly,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD05_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (outbound, _rx) = HubOutbound::attached_loopback();
                    let registry = Arc::new(Registry::new());
                    let store = Arc::new(HandleStore::new(64 * 1024));
                    let mailer = Mailer::new(registry, store).with_outbound(outbound);
                    let queue = install_test_trace_handle(&mailer);
                    mailer.push(
                        Mail::new(MailboxId(0xDEAD_BEEF_0002), KindId(0xFEED), vec![], 1)
                            .with_lineage(mail_id, mail_id, None),
                    );
                    (mail_id, drain_events_for(&queue, sender))
                }),
            },
            // 6. Ref-walk Err (malformed payload, terminal drop) —
            // Finished only.
            Case {
                name: "Ref-walk Err",
                expect: Expect::FinishedOnly,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD06_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (registry, mailer, _store) = make_mailer();
                    let queue = install_test_trace_handle(&mailer);
                    let kind_id = registry
                        .register_kind_with_descriptor(KindDescriptor {
                            name: HeldNote::NAME.into(),
                            schema: held_note_schema(),
                        })
                        .unwrap();
                    let sink = CapturingSink::new();
                    let id =
                        registry.register_inline("test.meta.refwalk_err", sink.inline_handler());
                    // Truncated payload (1 byte) — walker bails Err.
                    mailer.push(
                        Mail::new(id, kind_id, vec![0u8; 1], 1)
                            .with_lineage(mail_id, mail_id, None),
                    );
                    (mail_id, drain_events_for(&queue, sender))
                }),
            },
            // 7. Ref-walk Parked (held for handle publish) — neither.
            Case {
                name: "Ref-walk Parked",
                expect: Expect::HeldNeither,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD07_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (registry, mailer, _store) = make_mailer();
                    let queue = install_test_trace_handle(&mailer);
                    let outer_kind_id = registry
                        .register_kind_with_descriptor(KindDescriptor {
                            name: HeldNote::NAME.into(),
                            schema: held_note_schema(),
                        })
                        .unwrap();
                    let inner_kind_id = registry
                        .register_kind_with_descriptor(KindDescriptor {
                            name: Note::NAME.into(),
                            schema: note_schema(),
                        })
                        .unwrap();
                    let sink = CapturingSink::new();
                    let id =
                        registry.register_inline("test.meta.refwalk_parked", sink.inline_handler());
                    let held = HeldNote {
                        held: Ref::Handle {
                            id: 0x8888_8888_8888_8888,
                            kind_id: inner_kind_id.0,
                        },
                        seq: 1,
                    };
                    let payload = postcard::to_allocvec(&held).unwrap();
                    mailer.push(
                        Mail::new(id, outer_kind_id, payload, 1)
                            .with_lineage(mail_id, mail_id, None),
                    );
                    (mail_id, drain_events_for(&queue, sender))
                }),
            },
            // 8. CHASSIS_MAILBOX_ID with router installed — bracket.
            Case {
                name: "Chassis router installed",
                expect: Expect::Bracket,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD08_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (_registry, mailer, _store) = make_mailer();
                    let queue = install_test_trace_handle(&mailer);
                    mailer.install_chassis_router(Box::new(|_| {}));
                    mailer.push(
                        Mail::new(MailboxId::CHASSIS_MAILBOX_ID, KindId(0xFEED), vec![], 1)
                            .with_lineage(mail_id, mail_id, None),
                    );
                    (mail_id, drain_events_for(&queue, sender))
                }),
            },
            // 9. CHASSIS_MAILBOX_ID with no router — Finished only.
            Case {
                name: "Chassis router missing",
                expect: Expect::FinishedOnly,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD09_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (_registry, mailer, _store) = make_mailer();
                    let queue = install_test_trace_handle(&mailer);
                    mailer.push(
                        Mail::new(MailboxId::CHASSIS_MAILBOX_ID, KindId(0xFEED), vec![], 1)
                            .with_lineage(mail_id, mail_id, None),
                    );
                    (mail_id, drain_events_for(&queue, sender))
                }),
            },
        ];

        for case in cases {
            let name = case.name;
            let expect = case.expect;
            let (mail_id, events) = (case.run)();
            let received = events
                .iter()
                .any(|e| matches!(e, TraceEvent::Received { mail_id: m, .. } if *m == mail_id));
            let finished = events
                .iter()
                .any(|e| matches!(e, TraceEvent::Finished { mail_id: m, .. } if *m == mail_id));
            match expect {
                Expect::Bracket => assert!(
                    received && finished,
                    "{name}: expected Received+Finished bracket; got received={received} finished={finished}; events={events:?}"
                ),
                Expect::FinishedOnly => assert!(
                    !received && finished,
                    "{name}: expected Finished only (no Received); got received={received} finished={finished}; events={events:?}"
                ),
                Expect::HeldNeither => assert!(
                    !received && !finished,
                    "{name}: expected neither Received nor Finished (mail held); got received={received} finished={finished}; events={events:?}"
                ),
                Expect::NeitherFromMailer => assert!(
                    !received && !finished,
                    "{name}: expected neither (actor dispatch owns bracket downstream); got received={received} finished={finished}; events={events:?}"
                ),
            }
        }
    }
}
