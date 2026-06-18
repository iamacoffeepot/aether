// A loaded WASM component: its wasmtime `Store<ComponentCtx>`, instance,
// and the cached handles needed to deliver mail. Every payload is written
// into a region the host obtains from the guest's generic allocator
// (`realloc_p32`): a small fitting payload into a cached reused region, a
// larger one into an on-demand region grown to fit (ADR-0095).
//
// Holds the `ComponentCtx` (per-component context stored as wasmtime
// `Store` data) and `StateBundle` (ADR-0016 state-migration payload)
// alongside the `Component` itself — the ctx is the runtime half of
// the same primitive, so it lives here rather than in a separate
// module.

use std::cell::Cell;
use std::sync::Arc;

use wasmtime::{Engine, Linker, Memory, Module, Store, TypedFunc};

use crate::actor::native::binding::NativeBinding;
use crate::actor::wasm::reply_table::{NO_REPLY_HANDLE, ReplyEntry, ReplyTable};
use crate::mail::mailer::Mailer;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::OwnedDispatch;
use crate::mail::registry::{MailboxEntry, Registry};
use crate::mail::{Mail, MailId, MailKind, MailRef, MailboxId, Source, SourceAddr};
use crate::scheduler::pending_depth;

// ADR-0095: the substrate delivers every host→guest payload — mail, init
// config (ADR-0090), rehydrate state (ADR-0016) — through the guest's generic
// allocator (`realloc_p32`), never a host-chosen offset. It holds two regions
// per component, obtained from that one allocator and reused under the
// run-token invariant (one payload of each class live at a time, consumed
// synchronously inside the guest entry point):
//
//   - a SMALL region of `SMALL_REGION_BYTES` bytes, allocated once at instantiate and
//     cached, that the host writes a fitting payload into directly — no
//     per-payload allocator call;
//   - a LARGE region grown on demand to the largest over-floor payload the
//     component has received, reused thereafter.
//
// Neither region is freed — wasm has no `memory.shrink`, so a free reclaims
// nothing. A payload past `MAX_DELIVERABLE_MAIL_BYTES` is rejected (dropped for
// mail, boot-error for config / state). Region selection is `Component::place`.

/// Alignment requested from the guest allocator for delivery regions. Payloads
/// are raw byte buffers the guest reads via `slice::from_raw_parts`; 8 bytes
/// covers any in-place pod read a guest might do.
const DELIVERY_ALIGN: u32 = 8;

/// Size (bytes) of the always-allocated SMALL delivery region. A payload at or
/// below this writes directly to the cached small pointer with no per-payload
/// allocator call; a larger one grows the LARGE region. Every component pays
/// this once, so it is kept modest — most substrate mail (tick, key,
/// window-size, camera) is tens of bytes, well under it — while still covering
/// typical small config / state without spilling. Tunable: raising it reduces
/// spillover at the cost of more per-component memory across many components.
const SMALL_REGION_BYTES: usize = 8 * 1024;

/// Absolute ceiling on inbound payload bytes the substrate will deliver at all
/// (iamacoffeepot/aether#1337). A payload past this is dropped (mail) or
/// rejected (config / state) with a loud log rather than asking the guest to
/// allocate a buffer that could exhaust its memory and trap. The wire frame cap
/// bounds arrivals upstream — this is defense in depth. 64 MiB matches the
/// default `AETHER_MAX_FRAME_SIZE`.
const MAX_DELIVERABLE_MAIL_BYTES: usize = 64 << 20;

/// ADR-0016 §3: opt-in state migration payload. The substrate owns the
/// buffer from the moment `save_state` is called on the old instance
/// until the bundle is handed to the new instance via `on_rehydrate`
/// (or discarded if no successor consumes it). Both fields are opaque
/// to the substrate — the component owns versioning and the byte layout.
#[derive(Debug, Clone)]
pub struct StateBundle {
    pub version: u32,
    pub bytes: Vec<u8>,
}

/// Per-component context stored as wasmtime `Store` data. Holds the
/// sender's own `MailboxId`, a handle to the shared mail queue, and a
/// handle to the registry so the `send_mail` host function can route
/// without consulting the scheduler's internals.
///
/// Deliberately does NOT hold the scheduler's full shared state — doing
/// so would create an Arc cycle through `Scheduler owns Actor, Actor
/// owns Store<ComponentCtx>, ComponentCtx back to Scheduler`. By holding
/// only `Arc<Registry>` and `Arc<Mailer>` the cycle is broken: neither
/// of those owns any actor.
pub struct ComponentCtx {
    pub sender: MailboxId,
    pub registry: Arc<Registry>,
    pub queue: Arc<Mailer>,
    /// ADR-0013: direct outbound handle so the `reply_mail` host fn
    /// can address a specific Claude session without routing through
    /// a well-known sink. Broadcast still goes through
    /// `hub.claude.broadcast`; reply is the session-targeted twin.
    /// `HubOutbound::disconnected` when no hub is attached — sends
    /// silently drop, matching the broadcast semantics.
    pub outbound: Arc<HubOutbound>,
    /// ADR-0013 + ADR-0017: handle→entry map populated by
    /// `Component::deliver` whenever an inbound mail has a meaningful
    /// reply target — a Claude session (`ReplyEntry::Session`) or
    /// another component (`ReplyEntry::Component`). The guest
    /// receives an opaque `u32` handle as the 4th param on its
    /// `receive` shim and passes it back to `reply_mail`; the
    /// substrate routes either over `HubOutbound` or back through
    /// `Mailer` based on the variant.
    pub reply_table: ReplyTable,
    /// Set by the `save_state` host fn during `on_dehydrate`. The
    /// substrate extracts it after hooks return via
    /// `Component::take_saved_state`. Never read by the guest —
    /// rehydration reads from a scratch offset written by the
    /// substrate, not from here.
    pub saved_state: Option<StateBundle>,
    /// Set by the `save_state` host fn when it rejects a call (1 MiB
    /// cap exceeded, OOB pointer). ADR-0016 §4: a failing save aborts
    /// the replace; the substrate checks this after `on_dehydrate` and
    /// surfaces the message back up the control plane.
    pub save_state_error: Option<String>,
    /// Set by the `init_failed_p32` host fn when the guest's `init`
    /// returns `Err(BootError)`. Issue 525 Phase 4b / issue 531: the
    /// substrate reads this after `init` returns non-zero and
    /// surfaces the message in `LoadResult::Err { error }`. The guest
    /// stages the bytes here and returns 1 from its `init` shim;
    /// `Component::instantiate` turns the staged message into a
    /// `wasmtime::Error` so the existing load-failure path in
    /// `dispatch_load_component` reports it like any other
    /// instantiation error. None on the success path.
    pub init_failure: Option<String>,
    /// Trampoline binding the reply / outbound-mail host fns route
    /// through (the binding owns the actor's inbox + reply machinery +
    /// correlation counter). `Some` for ctx instances built by
    /// `WasmTrampoline::init` (in `aether-capabilities`; issue 634
    /// Phase 4 PR 3); `None` for the test paths that build
    /// `ComponentCtx` without a real trampoline.
    pub binding: Option<Arc<NativeBinding>>,
    /// ADR-0042 correlation counter. Per-component (one
    /// `ComponentCtx` per component instance). Holds the *next* id
    /// to mint; `prev_correlation()` reads `counter - 1` to return
    /// the last one minted. Starts at `1` so that `0` always means
    /// "no correlation" (backward-compat sentinel for replies that
    /// don't filter on correlation, and for `prev_correlation` before
    /// any send).
    ///
    /// `Cell` instead of `AtomicU64`: the component is single-
    /// threaded (ADR-0038 actor-per-component), so the counter is
    /// never touched from multiple threads.
    correlation_counter: Cell<u64>,
    /// ADR-0080 §5 in-flight inbound `MailId`. Set by
    /// [`Component::deliver`] before invoking the guest's
    /// `receive_p32` shim so any [`ComponentCtx::send`] the guest
    /// triggers stamps `parent_mail = Some(in_flight_mail_id)` and
    /// `inherited_root = Some(in_flight_root)`. Cleared back to
    /// [`MailId::NONE`] when `receive_p32` returns. Issue
    /// iamacoffeepot/aether#722.
    in_flight_mail_id: Cell<MailId>,
    /// ADR-0080 §5 in-flight inbound `root`. See `in_flight_mail_id`.
    in_flight_root: Cell<MailId>,
    /// Issue iamacoffeepot/aether#1465: lineage-`MailId` counter for
    /// [`ComponentCtx::reply`]. A reply echoes the inbound correlation
    /// on its `reply_to` (so it correlates home), but its own trace
    /// `MailId` needs a fresh identity disjoint from this component's
    /// `send` mints: `build_tree` keys trace nodes by `MailId`, so a
    /// reply whose lineage id equaled one of this component's sends
    /// (both inherit the same inbound root) would collapse two nodes
    /// into one. This counter starts at [`REPLY_LINEAGE_BASE`] — above
    /// the `send` correlation space (`mint_correlation`, from `1`) — so
    /// the two never overlap. It is deliberately separate from
    /// `correlation_counter`: `prev_correlation_p32` reports a guest's
    /// own request correlations, and a reply is not one of them.
    reply_lineage_counter: Cell<u64>,
    /// ADR-0097: a sibling-spawn request staged by the `spawn_sibling`
    /// host fn and drained by the trampoline after `receive_p32`
    /// returns — the same host-fn-stages / host-drains pattern as
    /// `saved_state`. `None` outside an in-flight spawn. The trampoline
    /// performs the actual `spawn_child::<WasmTrampoline>`; substrate
    /// can't name that capabilities-layer type (ADR-0097 §4).
    pub pending_spawn: Option<PendingSpawn>,
}

/// The mailbox-name prefix every wasm component (loaded or spawned)
/// registers under: `aether.embedded:<name>` — the embedding-host class
/// namespace (ADR-0099 §5/§6). The `spawn_sibling` host fn (ADR-0097)
/// needs this string to predict a spawned sibling's
/// `MailboxId = fold(host_carry, hash("{prefix}:{subname}"))`
/// synchronously. It **forward-feeds** the sole owner of the literal,
/// [`EmbeddedHost`](aether_actor::EmbeddedHost), which sits below this
/// crate, so substrate and the capabilities-layer `WasmTrampoline` now
/// reference one const instead of mirroring two literals; capabilities'
/// `trampoline_namespace_matches_substrate` test guards the match.
pub const TRAMPOLINE_NAMESPACE: &str =
    <aether_actor::EmbeddedHost as aether_actor::Actor>::NAMESPACE;

/// ADR-0097: a sibling-spawn request the `spawn_sibling` host fn stages
/// onto [`ComponentCtx`] for the trampoline to drain and execute.
/// `tag` selects the exported type at `init_typed_p32`; `subname` is the
/// full trampoline subname (the spawned instance addresses as
/// `aether.embedded:<subname>`); `config` is the encoded
/// `Config` kind handed to the new instance.
#[derive(Debug, Clone)]
pub struct PendingSpawn {
    pub tag: u64,
    pub subname: String,
    pub config: Vec<u8>,
}

/// Issue iamacoffeepot/aether#1465: starting value of
/// [`ComponentCtx::reply_lineage_counter`]. Sits at the top half of the
/// `u64` space, above the `send` correlation counter (which starts at
/// `1` and increments once per send), so a reply's lineage `MailId`
/// never collides with one this component minted for a `send`. A run
/// would need `2^63` sends to reach this base, so the two spaces stay
/// disjoint in practice.
const REPLY_LINEAGE_BASE: u64 = 1 << 63;

impl ComponentCtx {
    /// Build a fresh ctx with empty state-migration slots and an
    /// empty sender table. Using this over the struct literal keeps
    /// the private fields (`reply_table`, `saved_state`,
    /// `save_state_error`) internal to the wiring — callers should
    /// never set them directly.
    pub fn new(
        sender: MailboxId,
        registry: Arc<Registry>,
        queue: Arc<Mailer>,
        outbound: Arc<HubOutbound>,
    ) -> Self {
        Self {
            sender,
            registry,
            queue,
            outbound,
            reply_table: ReplyTable::new(),
            saved_state: None,
            save_state_error: None,
            init_failure: None,
            binding: None,
            correlation_counter: Cell::new(1),
            in_flight_mail_id: Cell::new(MailId::NONE),
            in_flight_root: Cell::new(MailId::NONE),
            reply_lineage_counter: Cell::new(REPLY_LINEAGE_BASE),
            pending_spawn: None,
        }
    }

    /// Wire the trampoline's `NativeBinding` into the ctx so the
    /// reply / outbound-mail host fns (in
    /// [`crate::actor::wasm::host_fns`]) can route through it. Called
    /// by `WasmTrampoline::init` (in
    /// `aether-capabilities`) right after constructing the ctx and before
    /// `Component::instantiate` — the host-fn closure captures the ctx
    /// via the wasmtime `Store` data pointer at instantiation time,
    /// not at host-fn call time, so installing later than that is
    /// fine. Promoted from `pub(crate)` to `pub` by issue 654 when the
    /// trampoline moved to `aether-capabilities` next to its only
    /// consumer; no other call site exists today and none is intended.
    pub fn install_binding(&mut self, binding: Arc<NativeBinding>) {
        self.binding = Some(binding);
    }

    /// Mint the next correlation id and bump the counter. Private —
    /// callers that want a correlation use `ComponentCtx::send`,
    /// which mints internally and tags the outgoing mail.
    fn mint_correlation(&self) -> u64 {
        let id = self.correlation_counter.get();
        self.correlation_counter.set(id + 1);
        id
    }

    /// Issue iamacoffeepot/aether#1465: hand out the next lineage id for
    /// a [`Self::reply`]. Drawn from a counter disjoint from
    /// `mint_correlation` (see [`Self::reply_lineage_counter`]) so a
    /// reply's trace `MailId` never merges with one of this component's
    /// own sends, and so it leaves the guest-facing `prev_correlation`
    /// counter untouched.
    fn next_reply_lineage(&self) -> u64 {
        let id = self.reply_lineage_counter.get();
        self.reply_lineage_counter.set(id + 1);
        id
    }

    /// Issue 1987: resolve the dispatch identity outbound mail is stamped
    /// with from the `from` the guest carried on its send / reply. The
    /// caller (the `send_mail_p32` / `reply_mail_p32` host fn) has already
    /// validated `from` is in-cluster; [`MailboxId::NONE`] (a zero / foreign
    /// `from`, or a substrate-internal call site that bypasses the host fn,
    /// e.g. a test fixture) falls back to `self.sender` — the component's
    /// own id. For an inline child `from` is the child's alias, so its sends
    /// stamp the child's address; for a normally-addressed actor it is the
    /// component's own id, so the stamp is unchanged.
    fn dispatch_identity(&self, from: MailboxId) -> MailboxId {
        if from == MailboxId::NONE {
            self.sender
        } else {
            from
        }
    }

    /// Return the correlation id used by the most recent
    /// `ComponentCtx::send` call. The `prev_correlation_p32` host fn
    /// surfaces this to the guest so a handler can match an inbound
    /// reply to the request it sent. Returns `0` (the "no
    /// correlation" sentinel) before any send has been made.
    pub fn prev_correlation(&self) -> u64 {
        // counter holds the *next* id to mint; subtract to get the
        // last one. `.saturating_sub(1)` covers the pre-send case
        // where counter is still `1` (initial) → returns `0`.
        self.correlation_counter.get().saturating_sub(1)
    }

    /// Dispatch mail. If the recipient is a sink, the handler runs inline
    /// on the caller's thread. Otherwise defer to the mailer, which
    /// routes to the component's inbox, warn-drops dropped/unknown
    /// mailboxes, or bubbles unknown ids up to the hub-substrate when
    /// a `HubOutbound` is wired (ADR-0037).
    pub fn send(
        &self,
        recipient: MailboxId,
        kind: MailKind,
        payload: Vec<u8>,
        count: u32,
        from: MailboxId,
    ) {
        // ADR-0042: mint a fresh correlation_id for this send and
        // stash it on `last_correlation` so `prev_correlation_p32`
        // can return it to the guest. The minted id rides on the
        // outgoing `Source.correlation_id`; the reply's echo
        // (auto-routed by `Mailer::send_reply`) carries it back so a
        // handler can match the reply to this send.
        let correlation = self.mint_correlation();
        // Issue 1987: stamp origin from the dispatch identity the guest
        // carried on the send (`from`, validated in-cluster by the host fn)
        // so an inline child's sends carry the child's address; a
        // zero / foreign `from` falls back to `self.sender`.
        let identity = self.dispatch_identity(from);
        let reply_to = Source::with_correlation(SourceAddr::Component(identity), correlation);

        // ADR-0080 §1 (issue iamacoffeepot/aether#722): mint the
        // outbound's MailId from the same correlation that drives
        // reply routing — symmetric with `NativeBinding::send_mail_with_lineage`,
        // which uses one counter for both.
        let mail_id = MailId::new(identity, correlation);
        self.send_routed(
            recipient, kind, payload, count, reply_to, mail_id, false, identity,
        );
    }

    /// ADR-0080 §7 fire-and-forget escape hatch: the detached
    /// counterpart of [`Self::send`]. Routes the guest's send without
    /// inheriting the in-flight dispatch's lineage, so the recipient
    /// starts a fresh causal chain. Reached from the `send_mail_p32`
    /// host fn when the guest sets the detached flag (`FfiActorMailbox::
    /// send_detached`). Correlation / reply-routing are identical to
    /// `send` — only the trace lineage differs. `from` (issue 1987) is the
    /// guest-carried dispatch identity, resolved the same way as in `send`.
    pub fn send_detached(
        &self,
        recipient: MailboxId,
        kind: MailKind,
        payload: Vec<u8>,
        count: u32,
        from: MailboxId,
    ) {
        let correlation = self.mint_correlation();
        let identity = self.dispatch_identity(from);
        let reply_to = Source::with_correlation(SourceAddr::Component(identity), correlation);
        let mail_id = MailId::new(identity, correlation);
        self.send_routed(
            recipient, kind, payload, count, reply_to, mail_id, true, identity,
        );
    }

    /// Issue iamacoffeepot/aether#1465: correlation-preserving sibling
    /// of [`Self::send`] for the `reply_mail_p32` `SourceAddr::Component`
    /// arm. A reply must echo the inbound mail's `correlation` so the
    /// originating actor (or the RPC server's `in_flight` table) can
    /// match it home — the ADR-0042 contract the `Session` /
    /// `EngineMailbox` arms and native `Mailer::send_reply` already
    /// honor. So it stamps `reply_to = Source::with_correlation(
    /// SourceAddr::None, correlation)` — the echo, with reply-of-a-reply
    /// target `None` — rather than `send`'s fresh-minted
    /// `Component(self)`.
    ///
    /// It routes through the same [`Self::send_routed`] body as `send`,
    /// so a guest's reply stays a first-class child of the inbound mail
    /// in the trace + settlement chain (symmetric with the guest's other
    /// sends). Two things differ from `send`: the `reply_to` above, and
    /// the lineage `MailId`, which comes from [`Self::next_reply_lineage`]
    /// (disjoint from the `send` correlation space) instead of
    /// `mint_correlation` — a reply is not the component's own outbound
    /// request, so it must not advance the counter `prev_correlation_p32`
    /// reports.
    pub(crate) fn reply(
        &self,
        recipient: MailboxId,
        kind: MailKind,
        payload: Vec<u8>,
        count: u32,
        correlation: u64,
        from: MailboxId,
    ) {
        let reply_to = Source::with_correlation(SourceAddr::None, correlation);
        // Issue 1987: a child's reply stamps the child's identity (the
        // guest-carried `from`, validated in-cluster by the host fn) on its
        // lineage `MailId`, like its sends; a zero / foreign `from` falls
        // back to `self.sender`.
        let identity = self.dispatch_identity(from);
        let mail_id = MailId::new(identity, self.next_reply_lineage());
        self.send_routed(
            recipient, kind, payload, count, reply_to, mail_id, false, identity,
        );
    }

    /// Shared routing body of [`Self::send`] and [`Self::reply`]: stamp
    /// the inbound lineage, fire the ADR-0080 §2 `Sent` hook, then
    /// dispatch by recipient class (inline sink, actor inbox, or
    /// dropped/unknown bubble-up). The caller supplies the `reply_to`
    /// (fresh `Component(self)` correlation for a send, echoed inbound
    /// correlation with target `None` for a reply) and the lineage
    /// `mail_id`.
    ///
    /// `force_detach` (ADR-0080 §7) suppresses the in-flight lineage
    /// inheritance: `true` (a guest `send_detached`) starts a fresh
    /// causal chain regardless of the in-flight cells; `false` (the
    /// default `send` / a reply) inherits the dispatch's chain.
    // The arg list is the routing surface `send` / `send_detached` /
    // `reply` all funnel through; bundling it into a struct would only
    // move the same fields one indirection away with no call-site win.
    // `identity` is the resolved dispatch identity (issue 1987) — the
    // caller computed it from the guest-carried `from`, so the recorded
    // source + the `origin` name read it directly.
    #[allow(clippy::too_many_arguments)]
    fn send_routed(
        &self,
        recipient: MailboxId,
        kind: MailKind,
        payload: Vec<u8>,
        count: u32,
        reply_to: Source,
        mail_id: MailId,
        force_detach: bool,
        identity: MailboxId,
    ) {
        // ADR-0080 §1 (issue iamacoffeepot/aether#722): the in-flight
        // cells were populated by `Component::deliver` for guest-triggered
        // sends (and remain `NONE` for substrate-internal call sites that
        // bypass `deliver`, e.g. test fixtures). ADR-0080 §7: a detached
        // send ignores them and opens its own chain.
        let (parent_mail, inherited_root) = if force_detach {
            (None, None)
        } else {
            let parent_mail = match self.in_flight_mail_id.get() {
                id if id == MailId::NONE => None,
                id => Some(id),
            };
            let inherited_root = match self.in_flight_root.get() {
                id if id == MailId::NONE => None,
                id => Some(id),
            };
            (parent_mail, inherited_root)
        };
        let root = inherited_root.unwrap_or(mail_id);
        // Issue 1987: the recorded source + the `origin` name stamped below
        // read the dispatch `identity` the caller resolved from the guest's
        // `from`, so an inline child's mail is attributed to the child's
        // address; a normally-addressed actor's is its own id.
        self.queue
            .record_sent(mail_id, root, parent_mail, identity, recipient, kind);

        // Closure-bound (actor-enqueue) and Sink-bound (synchronous
        // handler) recipients dispatch inline here, bypassing the
        // mailer's full route. Issue 838: `Sink` gets a
        // `Received`/`Finished` bracket so the chain's `in_flight`
        // balances; `Closure` does NOT because the actor's
        // downstream dispatch loop records the bracket. See
        // [`MailboxEntry`] docs for the contract.
        match self.registry.entry(recipient) {
            Some(MailboxEntry::Inbox { handler, .. }) => {
                let kind_name = self.registry.kind_name(kind).unwrap_or_default();
                // Component-originated mail: the sender is this ctx's
                // mailbox, so its registry name is the `origin` any
                // sink cares about (ADR-0011), and the same mailbox id
                // rides on `reply_to.addr` so sink handlers that want
                // to reply (ADR-0041's io sink is the motivating case)
                // can route `*Result` back to this component via
                // `Mailer::send_reply`.
                //
                // iamacoffeepot/aether#848: handler is
                // `Arc<dyn InboxHandler>`; build an [`OwnedDispatch`]
                // and move payload + kind_name into it. The bytes
                // flow straight into the downstream cap's mpsc
                // envelope without a `to_vec()` clone.
                let origin = self.registry.mailbox_name(identity);
                // ADR-0094: the second of two production mint sites
                // (ComponentCtx's inline send bypasses `route_mail`). Armed
                // here; the recipient actor's dispatcher discharges it.
                handler.enqueue(OwnedDispatch::armed(
                    kind,
                    kind_name,
                    origin,
                    reply_to,
                    MailRef::from(payload),
                    count,
                    mail_id,
                    root,
                    parent_mail,
                    // iamacoffeepot/aether#1134: the second production
                    // deposit chokepoint (ComponentCtx's inline send
                    // bypasses `route_mail`), so stamp the deposit instant
                    // + scheduler backlog here too — else the recipient's
                    // `Received` would read a zeroed `t_enqueue`.
                    self.queue.now_nanos(),
                    pending_depth(),
                    recipient,
                ));
                return;
            }
            Some(MailboxEntry::Inline(handler)) => {
                let kind_name = self.registry.kind_name(kind).unwrap_or_default();
                let origin = self.registry.mailbox_name(identity);
                handler.dispatch(crate::mail::registry::MailDispatch {
                    kind,
                    kind_name: &kind_name,
                    origin: origin.as_deref(),
                    sender: reply_to,
                    payload: &payload,
                    count,
                    mail_id,
                    root,
                    parent_mail,
                });
                // ADR-0080 §2 settlement hook. Inline mailboxes have no
                // per-actor trace ring, so post-ADR-0086 Phase 3c their
                // Received/Finished trace events aren't recorded — only
                // settlement accounting runs here.
                self.queue.record_finished(mail_id, root);
                return;
            }
            Some(MailboxEntry::Dropped) | None => {
                // Falls through to the `self.queue.push` path below
                // — Dropped warn-drops in `route_mail` (with the
                // Finished bracket from issue 839); unknown bubbles
                // up via ADR-0037 (also with the local-side
                // Finished from issue 839).
            }
        }

        // Dropped / unknown both funnel through `Mailer::push`:
        // - Dropped: warn-drops in `route_mail`.
        // - Unknown (ADR-0037): bubbles up to the hub-substrate via
        //   `MailToHubSubstrate`; the `source_mailbox_id` it carries is
        //   recovered from `reply_to.addr` when it's a Component
        //   variant (warn-drops otherwise).
        self.queue.push(
            Mail::new(recipient, kind, payload, count)
                .with_reply_to(reply_to)
                .with_lineage(mail_id, root, parent_mail),
        );
    }

    /// Set the in-flight `(mail_id, root)` context the next
    /// [`Self::send`] will read for `parent_mail` + `inherited_root`.
    /// Called by [`Component::deliver`] right before the guest's
    /// `receive_p32` shim runs. Pre-issue-722 `ComponentCtx::send`
    /// stamped [`MailId::NONE`]; setting these cells ahead of the call
    /// makes guest-triggered sends visible to the trace observer with
    /// the correct parent edge.
    pub(crate) fn set_in_flight(&self, mail_id: MailId, root: MailId) {
        self.in_flight_mail_id.set(mail_id);
        self.in_flight_root.set(root);
    }

    /// Clear the in-flight context after the guest's `receive_p32`
    /// shim returns. Symmetric with [`Self::set_in_flight`].
    pub(crate) fn clear_in_flight(&self) {
        self.in_flight_mail_id.set(MailId::NONE);
        self.in_flight_root.set(MailId::NONE);
    }
}

/// Sentinel the ADR-0033 `#[actor]` dispatcher returns from
/// `receive_p32` when mail arrives with a kind id the component has
/// no typed handler for and no fallback. Substrate-side, the
/// scheduler turns this into a `tracing::warn!` so the unhandled
/// kind surfaces in `engine_logs` without aborting the run. Strict-
/// receiver enforcement at the substrate (pre-delivery rejection)
/// is deferred to a later ADR; Phase 2 is warnings only.
pub const DISPATCH_UNKNOWN_KIND: u32 = 1;

/// Sentinel [`Component::deliver`] returns when it refused to deliver an
/// inbound — its payload exceeded the deliverable ceiling, or the guest exports
/// no allocator (ADR-0095). The mail was dropped (logged) without touching
/// guest memory or invoking `receive`; the caller treats it as a non-error so
/// the native dispatcher still discharges settlement.
pub const DISPATCH_DROPPED_OVERSIZE: u32 = 2;

/// Contract with the guest: it exports a
/// `receive(kind, ptr, byte_len, count, sender, recipient) -> u32`
/// entrypoint and a `memory` named `memory`. ADR-0013 widened the
/// receive ABI with a `sender: u32` parameter — a per-instance handle
/// the guest can pass back to `reply_mail`, or `NO_REPLY_HANDLE` for
/// component-originated mail. ADR-0114 decision #1 added the trailing
/// `recipient: u64` — the mailbox id the substrate routed this mail to
/// (the actor's own id for a normal actor; an inline-child alias for
/// the membrane). The `byte_len: u32` parameter (added
/// to support postcard-shaped receivers per ADR-0033's "any declared
/// kind" intent) is the total payload size the substrate wrote at
/// `ptr`, sourced from `mail.payload.len()`. Cast decoders sanity-
/// check it against `size_of::<K>() * count`; postcard decoders use
/// it as the exact slice length so a parser bug or a corrupted frame
/// can't read past the substrate-written bytes into adjacent linear
/// memory. ADR-0015 + issue 584 add optional `wire`, `unwire`,
/// `on_dehydrate`, and `on_rehydrate` exports; the substrate calls
/// them at the right lifecycle moments when present and silently
/// skips when absent (no-op trait defaults compile down to no symbol
/// under LTO, so components that don't override stay
/// backwards-compat).
/// The guest's generic delivery allocator export (`realloc_p32`,
/// `cabi_realloc`-shaped): `(old_ptr, old_size, align, new_size) -> ptr`.
type ReallocFunc = TypedFunc<(u32, u32, u32, u32), u32>;

/// The guest's mail-dispatch export (`receive_p32`):
/// `(kind, ptr, byte_len, count, sender, recipient, source) -> rc`. The
/// trailing `recipient` (ADR-0114) and `source` (issue 2001) frame slots
/// thread the routed address and the resolved inbound source to the guest.
type ReceiveFunc = TypedFunc<(u64, u32, u32, u32, u32, u64, u64), u32>;

pub struct Component {
    store: Store<ComponentCtx>,
    memory: Memory,
    receive: ReceiveFunc,
    /// Issue 584 Phase 2b: post-init mail-allowed hook. Stored (rather
    /// than called inside [`Self::instantiate`]) so the trampoline
    /// can fire it AFTER its mailbox is registered — issue 640
    /// Phase 2 surfaced a race where `wire`-time `subscribe_input`
    /// mail was rejected by the input cap's
    /// `validate_subscriber_mailbox` because the trampoline mailbox
    /// hadn't been registered yet (init runs in
    /// `spawn_actor` step 4, registration is step 5–7).
    /// `WasmTrampoline::wire` invokes [`Self::wire`] post-registration.
    wire: Option<TypedFunc<u64, u32>>,
    /// Issue 584 Phase 2b: pre-shutdown mail-allowed hook. Called by
    /// the trampoline (via [`Self::unwire`]) before `on_dehydrate` on
    /// the dying instance, or before the `Component` value drops on a
    /// `DropComponent`.
    unwire: Option<TypedFunc<u64, u32>>,
    on_dehydrate: Option<TypedFunc<(), u32>>,
    on_rehydrate: Option<TypedFunc<(u32, u32, u32), u32>>,
    /// ADR-0095: the guest's generic delivery allocator
    /// (`realloc_p32`, `cabi_realloc`-shaped). Every payload — mail, config,
    /// state — is written into a region obtained from it. `None` for a
    /// non-conforming guest that exports no allocator; such a guest can't
    /// receive any payload (delivery drops with a loud log).
    realloc: Option<ReallocFunc>,
    /// SMALL delivery region: `SMALL_REGION_BYTES` bytes, allocated once at
    /// instantiate from [`Self::realloc`] and cached. Non-null when an
    /// allocator is present; `0` for a no-allocator guest. A payload that fits
    /// is written here directly with no per-payload allocator call.
    small_ptr: u32,
    /// LARGE delivery region: grown on demand to the largest over-floor payload
    /// (`large_cap` bytes) and reused. `0` until the first such payload. The
    /// pointer is re-fetched from each grow, since a grow may relocate it.
    large_ptr: u32,
    /// Current capacity (bytes) of the LARGE region; `0` until first grown.
    large_cap: u32,
    /// Mailbox id stamped at instantiate-time, replayed into `wire`
    /// and `unwire` calls. Same value the guest's `init` shim received.
    self_mailbox_id: u64,
}

/// Result of choosing a destination region for a host→guest payload
/// ([`Component::place`]).
enum Placement {
    /// Write the payload at this guest pointer, then call the entry point. The
    /// pointer is non-null and `DELIVERY_ALIGN`-aligned, so a zero-length
    /// payload still yields a valid pointer for the guest's slice construction.
    At(u32),
    /// Payload exceeds [`MAX_DELIVERABLE_MAIL_BYTES`]; the caller drops (mail)
    /// or rejects (config / state).
    Oversize,
    /// Guest exports no `realloc_p32` allocator (non-conforming guest); nothing
    /// can be delivered into it.
    NoAllocator,
}

impl Component {
    /// ADR-0095: choose the destination region for a host→guest payload of
    /// `len` bytes, growing the large region through the guest allocator when
    /// needed. A payload that fits `SMALL_REGION_BYTES` lands in the cached small
    /// region with no allocator call; a larger one grows the reused large
    /// region (re-fetching its pointer, since a grow may relocate it); one past
    /// the ceiling is [`Placement::Oversize`]. Takes the fields explicitly
    /// rather than `&mut self` so [`Self::instantiate`] can call it before the
    /// `Component` value exists.
    fn place(
        store: &mut Store<ComponentCtx>,
        realloc: Option<&ReallocFunc>,
        small_ptr: u32,
        large_ptr: &mut u32,
        large_cap: &mut u32,
        len: usize,
    ) -> wasmtime::Result<Placement> {
        let Some(realloc) = realloc else {
            return Ok(Placement::NoAllocator);
        };
        if len <= SMALL_REGION_BYTES {
            return Ok(Placement::At(small_ptr));
        }
        if len > MAX_DELIVERABLE_MAIL_BYTES {
            return Ok(Placement::Oversize);
        }
        // Wasm32 carries u32 byte lengths; `len <= MAX_DELIVERABLE_MAIL_BYTES`
        // (64 MiB) keeps the cast lossless.
        #[allow(clippy::cast_possible_truncation)]
        let new_cap = len as u32;
        if *large_cap < new_cap {
            let ptr = realloc.call(store, (*large_ptr, *large_cap, DELIVERY_ALIGN, new_cap))?;
            if ptr == 0 {
                return Err(wasmtime::Error::msg(format!(
                    "guest allocator returned null growing the delivery buffer to {new_cap} bytes"
                )));
            }
            *large_ptr = ptr;
            *large_cap = new_cap;
        }
        Ok(Placement::At(*large_ptr))
    }

    /// Place + write the init config into a delivery region (ADR-0095) and
    /// return the guest pointer to hand `init_with_config_p32`. Mirrors
    /// [`Self::deliver`]'s routing; a config past the ceiling, or to a guest
    /// with no allocator, is a clean boot `Err` (surfaces as `LoadResult::Err`)
    /// rather than a write or trap. Factored out of [`Self::instantiate`].
    fn place_init_config(
        store: &mut Store<ComponentCtx>,
        memory: &Memory,
        realloc: Option<&ReallocFunc>,
        small_ptr: u32,
        large_ptr: &mut u32,
        large_cap: &mut u32,
        config_bytes: &[u8],
    ) -> wasmtime::Result<u32> {
        match Self::place(
            store,
            realloc,
            small_ptr,
            large_ptr,
            large_cap,
            config_bytes.len(),
        )? {
            Placement::At(ptr) => {
                if !config_bytes.is_empty() {
                    memory.write(store, ptr as usize, config_bytes)?;
                }
                Ok(ptr)
            }
            Placement::Oversize => {
                Self::log_oversize_config(
                    store,
                    config_bytes.len(),
                    "exceeds the absolute deliverable bound",
                );
                Err(wasmtime::Error::msg(format!(
                    "guest init config of {} bytes exceeds the {MAX_DELIVERABLE_MAIL_BYTES}-byte deliverable bound",
                    config_bytes.len(),
                )))
            }
            Placement::NoAllocator => {
                Self::log_oversize_config(
                    store,
                    config_bytes.len(),
                    "guest exports no realloc_p32 allocator (raw-FFI guest)",
                );
                Err(wasmtime::Error::msg(format!(
                    "guest init config of {} bytes cannot be delivered: guest exports no realloc_p32 allocator",
                    config_bytes.len(),
                )))
            }
        }
    }

    /// Instantiate a component from a compiled `Module`. `ctx` becomes
    /// the store data and is what every host function call against this
    /// component will see.
    ///
    /// ADR-0090 (issue 1256): `config_bytes` is the wire-encoded
    /// `<FfiActor::Config as Kind>` payload threaded through to the
    /// guest's `init_with_config_p32` shim. Pass `&[]` for actors whose
    /// `Config = ()` or for the back-compat path (legacy `init` does
    /// not consume the bytes).
    ///
    /// ADR-0095: the config write routes through `place`, the same
    /// allocator-backed two-region path [`Component::deliver`] uses for mail. A
    /// config that fits the small region lands there; a larger one (up to the
    /// `MAX_DELIVERABLE_MAIL_BYTES` ceiling) grows the large region; a config
    /// past that ceiling, or to a guest with no allocator export, is a clean
    /// boot error (`LoadResult::Err`) — never a write or trap. Whichever pointer
    /// the config landed at is what `init_with_config_p32(mailbox_id, ptr, len)`
    /// receives.
    ///
    /// ADR-0096: `type_tag` selects which exported actor type a
    /// multi-actor module instantiates. `Some(tag)` calls the guest's
    /// `init_typed_p32(mailbox_id, tag, ptr, len)` — a missing export is
    /// a clean boot error (the module isn't multi-actor, or doesn't
    /// export the selected type). `None` is the entry-type / single-actor
    /// path: the substrate probes `init_with_config_p32`, then the legacy
    /// `init` shapes, exactly as before.
    #[allow(
        clippy::too_many_lines,
        reason = "one cohesive instantiate sequence: build instance, probe the \
                  delivery allocator, select + run the init shim, look up the \
                  optional lifecycle exports. Splitting it would thread a dozen \
                  store/region locals through a helper for no clarity gain."
    )]
    pub fn instantiate(
        engine: &Engine,
        linker: &Linker<ComponentCtx>,
        module: &Module,
        ctx: ComponentCtx,
        config_bytes: &[u8],
        type_tag: Option<u64>,
    ) -> wasmtime::Result<Self> {
        let mut store = Store::new(engine, ctx);
        let instance = linker.instantiate(&mut store, module)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("guest exports no `memory`"))?;
        let receive = instance.get_typed_func::<(u64, u32, u32, u32, u32, u64, u64), u32>(
            &mut store,
            "receive_p32",
        )?;

        // Optional `init(mailbox_id) -> u32` export: called once before
        // the first `receive`, handed the component's own mailbox id so
        // the SDK's typelist walker can auto-subscribe input kinds
        // (ADR-0030 Phase 2). Falls back to the legacy `init()` shape
        // so raw-FFI components predating the Phase 2 ABI still load —
        // they just don't get auto-subscribe, which they never did.
        //
        // ADR-0090 (issue 1256): the substrate probes `init_with_config_p32`
        // first (the post-#1256 ABI that carries config bytes), then
        // the `(u64) -> u32` shape (Phase 2), then the legacy `()` shape.
        // The order is deliberate — a `#[actor]`-built guest exports
        // both `init_with_config_p32` and the legacy `init(u64)`, so a substrate
        // that probes `init` first would silently skip the config path.
        //
        // Issue 525 Phase 4b / issue 531: a non-zero return value
        // means the guest's `FfiActor::init` returned `Err(BootError)`
        // and staged the message via `init_failed_p32`. Drain the
        // staged string off the ctx and surface it as a wasmtime
        // error so the existing `dispatch_load_component` failure
        // path reports it via `LoadResult::Err { error }` — same
        // shape as a wasm trap, just with a more informative message.
        let mailbox_id = store.data().sender.0;
        // ADR-0095: the guest's generic delivery allocator. Probed before the
        // config write because config delivery routes through it, exactly like
        // `deliver` routes mail. Present on macro-built guests (emitted by
        // `export!`); absent on a non-conforming guest, which then can't receive
        // any payload. The allocator is a module-level export ready right after
        // instantiation and independent of the actor's `init`.
        let realloc = instance
            .get_typed_func::<(u32, u32, u32, u32), u32>(&mut store, "realloc_p32")
            .ok();
        // Allocate the reused SMALL delivery region once, up front, and cache
        // its (non-null) pointer for the hot path. The LARGE region is grown
        // lazily by `place` only when a payload exceeds the small floor.
        let small_ptr = if let Some(realloc) = &realloc {
            #[allow(clippy::cast_possible_truncation)]
            let ptr = realloc.call(
                &mut store,
                (0, 0, DELIVERY_ALIGN, SMALL_REGION_BYTES as u32),
            )?;
            if ptr == 0 {
                return Err(wasmtime::Error::msg(
                    "guest allocator returned null for the small delivery region at instantiate",
                ));
            }
            ptr
        } else {
            0
        };
        let mut large_ptr: u32 = 0;
        let mut large_cap: u32 = 0;
        // Wasm32 ABI carries `u32` byte lengths; config bytes are
        // bounded by guest memory size (well below `u32::MAX`).
        #[allow(clippy::cast_possible_truncation)]
        let config_len = config_bytes.len() as u32;
        let init_rc = if let Some(type_tag) = type_tag {
            // ADR-0096: a multi-actor module loaded with an export
            // selector. The module exports `init_typed_p32`
            // (mailbox_id, type_tag, ptr, len); the tag picks which
            // exported type to construct. A guest without that export
            // either isn't a multi-actor module or was built against an
            // older SDK — a clean boot error, never a silent fall-through
            // to the entry-only `init_with_config_p32`.
            let init_typed = instance
                .get_typed_func::<(u64, u64, u32, u32), u32>(&mut store, "init_typed_p32")
                .map_err(|e| {
                    wasmtime::Error::msg(format!(
                        "export selector set but guest exports no `init_typed_p32` \
                         (not a multi-actor module?): {e}"
                    ))
                })?;
            // ADR-0095: same allocator-backed config write as the
            // entry path below.
            let config_ptr = Self::place_init_config(
                &mut store,
                &memory,
                realloc.as_ref(),
                small_ptr,
                &mut large_ptr,
                &mut large_cap,
                config_bytes,
            )?;
            Some(init_typed.call(&mut store, (mailbox_id, type_tag, config_ptr, config_len))?)
        } else if let Ok(init_with_config) =
            instance.get_typed_func::<(u64, u32, u32), u32>(&mut store, "init_with_config_p32")
        {
            // ADR-0095: route the config write through the allocator-backed
            // two-region path `deliver` uses. An empty config still needs a
            // valid (non-null) pointer for the guest's slice construction,
            // which the cached small region provides.
            let config_ptr = Self::place_init_config(
                &mut store,
                &memory,
                realloc.as_ref(),
                small_ptr,
                &mut large_ptr,
                &mut large_cap,
                config_bytes,
            )?;
            Some(init_with_config.call(&mut store, (mailbox_id, config_ptr, config_len))?)
        } else if let Ok(init) = instance.get_typed_func::<u64, u32>(&mut store, "init") {
            // Legacy Phase 2 fallback. Discards config bytes — only
            // safe for `Config = ()`. A typed-config guest that lands
            // on this branch was built against a post-#1256 SDK whose
            // `export!` always emits both shims; the legacy path here
            // is the back-compat for raw-FFI / pre-macro guests.
            Some(init.call(&mut store, mailbox_id)?)
        } else if let Ok(init) = instance.get_typed_func::<(), u32>(&mut store, "init") {
            Some(init.call(&mut store, ())?)
        } else {
            None
        };
        if let Some(rc) = init_rc
            && rc != 0
        {
            let msg =
                store.data_mut().init_failure.take().unwrap_or_else(|| {
                    format!("guest init returned {rc} without staging an error")
                });
            return Err(wasmtime::Error::msg(format!("guest init failed: {msg}")));
        }

        // ADR-0015 hook exports are optional. A component whose
        // `FfiActor::on_dehydrate` is the default no-op still emits the
        // symbol via `export!`, but a raw-FFI guest without the macro
        // won't. Either way: look it up, store `None` if missing.
        // (Issue 584 Phase 3 retired `on_drop` — `unwire` is the
        // pre-shutdown hook now.) Named save/restore-side so the two
        // locals don't read as a `de`/`re` minimal pair.
        let save_hook = instance
            .get_typed_func::<(), u32>(&mut store, "on_dehydrate")
            .ok();
        // ADR-0016: `on_rehydrate` takes `(version, ptr, len)` — the
        // substrate writes the state bytes into a delivery region (ADR-0095,
        // via `call_on_rehydrate`), then calls the shim with `(version, ptr, len)`.
        let restore_hook = instance
            .get_typed_func::<(u32, u32, u32), u32>(&mut store, "on_rehydrate_p32")
            .ok();
        // Issue 584 Phase 2b: optional wire/unwire exports. Both take
        // the component's own mailbox id (same as `init`) so the guest
        // ctx can self-address. Raw-FFI guests without the macro won't
        // emit them; macro-using guests with default no-op trait
        // bodies still emit the symbol (the shim just calls into the
        // default body).
        let unwire = instance
            .get_typed_func::<u64, u32>(&mut store, "unwire")
            .ok();

        // Issue 584 Phase 2b / Issue 640 Phase 2: store the `wire`
        // export rather than calling it here. `instantiate` runs
        // inside `spawn_actor` step 4 — BEFORE the trampoline mailbox
        // is registered (step 5–7). A wire-time send like
        // `aether.input.subscribe { mailbox: self.mailbox_id() }`
        // would race the input cap's `validate_subscriber_mailbox`
        // and warn-drop. `WasmTrampoline::wire` fires this hook
        // post-registration via the `NativeActor::wire` lifecycle
        // method. wire stays one-shot — the trampoline drops the
        // typed-func handle after the call.
        let wire = instance.get_typed_func::<u64, u32>(&mut store, "wire").ok();
        Ok(Self {
            store,
            memory,
            receive,
            wire,
            unwire,
            on_dehydrate: save_hook,
            on_rehydrate: restore_hook,
            realloc,
            small_ptr,
            large_ptr,
            large_cap,
            self_mailbox_id: mailbox_id,
        })
    }

    /// Invoke the guest's `wire` hook one-shot. The trampoline calls
    /// this from its `NativeActor::wire` body — i.e. after the
    /// trampoline's mailbox has been registered, so a wire-time
    /// `subscribe_input` mail is validated against a live closure
    /// entry rather than being warn-dropped. Idempotent across the
    /// `Option::take()` — calling twice is a guest-side bug, but a
    /// repeat call no-ops cleanly.
    pub fn wire(&mut self) -> wasmtime::Result<()> {
        let Some(wire_fn) = self.wire.take() else {
            return Ok(());
        };
        let mailbox_id = self.self_mailbox_id;
        let rc = wire_fn.call(&mut self.store, mailbox_id)?;
        if rc != 0 {
            return Err(wasmtime::Error::msg(format!(
                "guest wire returned non-zero rc {rc}"
            )));
        }
        Ok(())
    }

    /// Deliver a mail into the component's linear memory and invoke
    /// `receive`. Returns the guest's return value (contract is
    /// currently informational; host-visible errors propagate as
    /// `wasmtime::Error`).
    ///
    /// ADR-0013 + ADR-0017: a fresh sender handle is allocated from
    /// the per-instance `ReplyTable` for every inbound that has a
    /// meaningful reply target — a Claude session (non-NIL
    /// `SessionToken`), a remote engine mailbox, or a peer component
    /// (`reply_to.addr = SourceAddr::Component(_)` populated by
    /// `ComponentCtx::send` / `NativeBinding::send_mail`).
    /// Broadcast-origin and system-generated mail pass
    /// `NO_REPLY_HANDLE` so the guest's `mail.reply_handle()` accessor
    /// returns `None`.
    /// Resolve the inbound mail's source `MailboxId` for the trailing
    /// `receive_p32` frame slot (issue 2001). A peer-component origin
    /// (`SourceAddr::Component`) yields that mailbox's raw id; every other
    /// origin — session, remote engine, or no reply target — yields
    /// `MailboxId::NONE.0` (0). Mirrors what `source_of_p32` resolved from
    /// the reply table, but reads the inbound's `SourceAddr` directly (the
    /// same value the reply entry is built from) without a table lookup.
    fn resolve_inbound_source(addr: &SourceAddr) -> u64 {
        match addr {
            SourceAddr::Component(m) => m.0,
            _ => MailboxId::NONE.0,
        }
    }

    pub fn deliver(&mut self, mail: &Mail) -> wasmtime::Result<u32> {
        // ADR-0042: carry the incoming correlation through to the
        // ReplyEntry so a subsequent `reply_mail` echoes it on the
        // outgoing reply. Session / engine mail that didn't originate
        // a correlation carries 0 — fine, echo of 0 is a no-op.
        let correlation = mail.reply_to.correlation_id;
        let entry = match &mail.reply_to.addr {
            SourceAddr::Session(token) => {
                Some(ReplyEntry::new(SourceAddr::Session(*token), correlation))
            }
            SourceAddr::EngineMailbox {
                engine_id,
                mailbox_id,
            } => Some(ReplyEntry::new(
                SourceAddr::EngineMailbox {
                    engine_id: *engine_id,
                    mailbox_id: *mailbox_id,
                },
                correlation,
            )),
            SourceAddr::Component(m) => {
                Some(ReplyEntry::new(SourceAddr::Component(*m), correlation))
            }
            SourceAddr::None => None,
        };
        let handle = match entry {
            Some(e) => self.store.data_mut().reply_table.allocate(e),
            None => NO_REPLY_HANDLE,
        };
        // ADR-0095: choose where in guest memory the payload lands via the
        // guest allocator — a fitting payload into the cached small region, a
        // larger one into the grown large region, anything past the ceiling or
        // to a no-allocator guest dropped loudly. A drop returns `Ok` without
        // invoking `receive`, so the trampoline's `forward_to_wasm` returns
        // normally and the native dispatcher discharges the inbound's
        // settlement bracket — no corruption, no trap, no hung caller.
        let payload_len = mail.payload.len();
        // Wasm32 ABI carries `u32` byte lengths; only used in branches where
        // `payload_len <= MAX_DELIVERABLE_MAIL_BYTES`, so the cast can't lose data.
        #[allow(clippy::cast_possible_truncation)]
        let byte_len = payload_len as u32;
        let mail_ptr = match Self::place(
            &mut self.store,
            self.realloc.as_ref(),
            self.small_ptr,
            &mut self.large_ptr,
            &mut self.large_cap,
            payload_len,
        )? {
            Placement::At(ptr) => ptr,
            Placement::Oversize => {
                self.log_dropped_oversize(
                    mail,
                    payload_len,
                    "exceeds the absolute mail-size bound",
                );
                return Ok(DISPATCH_DROPPED_OVERSIZE);
            }
            Placement::NoAllocator => {
                self.log_dropped_oversize(
                    mail,
                    payload_len,
                    "guest exports no realloc_p32 allocator (raw-FFI guest)",
                );
                return Ok(DISPATCH_DROPPED_OVERSIZE);
            }
        };

        self.memory
            .write(&mut self.store, mail_ptr as usize, mail.payload.bytes())?;
        // ADR-0080 §5 (issue iamacoffeepot/aether#722): publish the
        // inbound's lineage on `ComponentCtx` so any guest-triggered
        // `send_mail_p32` / `reply_mail_p32` host fn — both routed
        // through `ComponentCtx::send` — can stamp the outgoing mail
        // with `parent_mail = Some(inbound.mail_id)` and inherit the
        // chain `root`. Cleared after the call so a future cap-side
        // call site that bypasses `deliver` (today: only test
        // fixtures) doesn't accidentally pick up stale lineage.
        self.store.data().set_in_flight(mail.mail_id, mail.root);
        // ADR-0114 decision #1: thread the routed recipient through to
        // the guest as a `receive_p32` frame slot so a guest handler (and
        // the inline-child membrane) can read which address the mail was
        // sent to. For a normally-addressed actor this equals the actor's
        // own mailbox id.
        //
        // Issue 2001: thread the resolved inbound source as the trailing
        // slot too, so the guest's `FfiCtx::source_mailbox` is a single
        // ctx-field read on both the in-place and top-level paths and the
        // `source_of_p32` host round-trip can be retired. Resolved exactly
        // as `source_of_p32` did — a peer-component origin yields its
        // `MailboxId`, every other origin yields `MailboxId::NONE`.
        let source = Self::resolve_inbound_source(&mail.reply_to.addr);
        let result = self.receive.call(
            &mut self.store,
            (
                mail.kind.0,
                mail_ptr,
                byte_len,
                mail.count,
                handle,
                mail.recipient.0,
                source,
            ),
        );
        self.store.data().clear_in_flight();
        result
    }

    /// Loudly log an inbound mail dropped by `deliver` because its payload
    /// could not be delivered safely (iamacoffeepot/aether#1337). The mail is
    /// dropped, not written; the caller settles via the native dispatcher.
    fn log_dropped_oversize(&self, mail: &Mail, payload_len: usize, reason: &str) {
        let kind_name = self
            .store
            .data()
            .registry
            .kind_name(mail.kind)
            .unwrap_or_default();
        tracing::error!(
            target: "aether_substrate::component",
            kind = %kind_name,
            kind_id = mail.kind.0,
            payload_bytes = payload_len,
            small_region_bytes = SMALL_REGION_BYTES,
            deliverable_cap_bytes = MAX_DELIVERABLE_MAIL_BYTES,
            reason,
            "dropping inbound mail; cannot deliver safely (see ADR-0095)",
        );
    }

    /// Loudly log an init config rejected by [`Component::instantiate`] (ADR-0095)
    /// because it could not be delivered safely — either past the absolute
    /// ceiling, or to a guest with no allocator export. Mirrors
    /// [`Self::log_dropped_oversize`]; the caller returns an `Err` that surfaces
    /// as `LoadResult::Err` rather than writing or trapping. Associated (no
    /// `&self`) because `instantiate` has no `Component` yet.
    fn log_oversize_config(store: &Store<ComponentCtx>, config_bytes: usize, reason: &str) {
        tracing::error!(
            target: "aether_substrate::component",
            mailbox_id = store.data().sender.0,
            config_bytes,
            small_region_bytes = SMALL_REGION_BYTES,
            deliverable_cap_bytes = MAX_DELIVERABLE_MAIL_BYTES,
            reason,
            "rejecting init config; cannot deliver safely (see ADR-0095)",
        );
    }

    /// Issue 584 Phase 2b (ADR-0079 amended): pre-shutdown mail-allowed
    /// hook. Invoked by the trampoline before `on_dehydrate` on the
    /// dying instance, or before the `Component` value drops on a
    /// `DropComponent`. Same trap containment as the other hooks —
    /// a guest panic doesn't stall teardown.
    pub fn unwire(&mut self) {
        if let Some(f) = self.unwire.clone()
            && let Err(e) = f.call(&mut self.store, self.self_mailbox_id)
        {
            tracing::error!(target: "aether_substrate::component", error = %e, "unwire hook trapped");
        }
    }

    /// Invoke the guest's `on_dehydrate` hook if it exports one.
    /// Wasmtime traps (guest panics, unreachable) are caught and
    /// logged rather than propagated — per ADR-0015, a panicking
    /// hook must not stall teardown.
    pub fn on_dehydrate(&mut self) {
        if let Some(f) = self.on_dehydrate.clone()
            && let Err(e) = f.call(&mut self.store, ())
        {
            tracing::error!(target: "aether_substrate::component", error = %e, "on_dehydrate hook trapped");
        }
    }

    /// Extract the state bundle the guest deposited via `save_state`
    /// during `on_dehydrate`. Returns `None` if `save_state` was never
    /// called (component doesn't implement migration, or the hook is
    /// a no-op). Called by the control plane *after* `on_dehydrate`
    /// runs on the old instance — the bundle has to outlive the
    /// store.
    pub fn take_saved_state(&mut self) -> Option<StateBundle> {
        self.store.data_mut().saved_state.take()
    }

    /// ADR-0097: drain the sibling-spawn request the guest staged via
    /// the `spawn_sibling` host fn during the just-returned `receive`.
    /// The trampoline calls this after `deliver` and performs the
    /// actual `spawn_child::<WasmTrampoline>`. Destructive — returns
    /// `None` once drained, and `None` when the guest didn't spawn.
    pub fn take_pending_spawn(&mut self) -> Option<PendingSpawn> {
        self.store.data_mut().pending_spawn.take()
    }

    /// Extract a failure recorded by `save_state` (size cap, OOB).
    /// `None` on clean saves and on components that didn't attempt a
    /// save. Checked by the control plane to decide whether to abort
    /// the replace (ADR-0016 §4).
    pub fn take_save_error(&mut self) -> Option<String> {
        self.store.data_mut().save_state_error.take()
    }

    /// Write the prior-state bytes into a delivery region (ADR-0095, via
    /// `place`) and invoke `on_rehydrate(version, ptr, len)`. Returns
    /// `Ok(())` if the instance doesn't export `on_rehydrate` (ADR-0016 §3: the
    /// bundle is silently discarded when no handler claims it).
    ///
    /// ADR-0016 §4 specifies that a trap here aborts the replace, so errors are
    /// propagated rather than contained (unlike `on_dehydrate` / `unwire`). A
    /// region that can't be allocated, or a bundle past the deliverable ceiling,
    /// propagates as an `Err` too.
    pub fn call_on_rehydrate(&mut self, bundle: &StateBundle) -> wasmtime::Result<()> {
        let Some(f) = self.on_rehydrate.clone() else {
            return Ok(());
        };
        let len = bundle.bytes.len();
        // Wasm32 ABI carries `u32` byte lengths; bundle bytes are
        // bounded by guest memory size (well below `u32::MAX`).
        #[allow(clippy::cast_possible_truncation)]
        let byte_len = len as u32;
        let ptr = match Self::place(
            &mut self.store,
            self.realloc.as_ref(),
            self.small_ptr,
            &mut self.large_ptr,
            &mut self.large_cap,
            len,
        )? {
            Placement::At(ptr) => ptr,
            Placement::Oversize => {
                return Err(wasmtime::Error::msg(format!(
                    "rehydrate state of {len} bytes exceeds the {MAX_DELIVERABLE_MAIL_BYTES}-byte deliverable bound"
                )));
            }
            Placement::NoAllocator => {
                return Err(wasmtime::Error::msg(
                    "cannot rehydrate state: guest exports no realloc_p32 allocator",
                ));
            }
        };
        if !bundle.bytes.is_empty() {
            self.memory
                .write(&mut self.store, ptr as usize, &bundle.bytes)?;
        }
        f.call(&mut self.store, (bundle.version, ptr, byte_len))?;
        Ok(())
    }

    /// Read a `u32` from guest linear memory at `offset`. Test-only
    /// accessor: the production mail path writes into an allocator
    /// region and the guest interprets the bytes — nothing in non-test
    /// code reads guest memory directly.
    ///
    /// # Panics
    /// Panics if the memory read fails — fail-fast per ADR-0063:
    /// tests construct the offset/length pair directly, so an
    /// out-of-bounds read is a test bug.
    #[cfg(test)]
    pub fn read_u32(&mut self, offset: usize) -> u32 {
        let mut buf = [0u8; 4];
        self.memory
            .read(&mut self.store, offset, &mut buf)
            .expect("test memory read");
        u32::from_le_bytes(buf)
    }

    /// Read `len` bytes from guest linear memory starting at `offset`.
    /// Test-only accessor for verifying that a rehydrate hook copied
    /// bytes to a known marker offset.
    ///
    /// # Panics
    /// Panics if the memory read fails — fail-fast per ADR-0063:
    /// tests construct the offset/length pair directly, so an
    /// out-of-bounds read is a test bug.
    #[cfg(test)]
    pub fn read_bytes(&mut self, offset: usize, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        self.memory
            .read(&mut self.store, offset, &mut buf)
            .expect("test memory read");
        buf
    }
}

#[cfg(test)]
// Tests hold the capture `Mutex` guard across the assertion block so
// the snapshot reads atomically against the concurrent push path.
#[allow(clippy::significant_drop_tightening)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests {
    use std::sync::Arc;

    use wasmtime::{Engine, Linker};

    use std::sync::Mutex;

    use super::*;
    use crate::actor::wasm::host_fns;
    use crate::handle_store::HandleStore;
    use crate::mail::MailboxId;
    use crate::mail::mailer::Mailer;
    use crate::mail::outbound::{EgressEvent, HubOutbound};
    use crate::mail::registry;
    use crate::mail::registry::OwnedDispatch;
    use crate::mail::registry::Registry;
    use aether_data::tagged_id::Tag;
    use std::sync::mpsc::Receiver;

    /// Captured `(mail_id, root, parent_mail)` triple for the
    /// lineage-propagation tests in this module.
    type LineageCapture = Arc<Mutex<Vec<(MailId, MailId, Option<MailId>)>>>;

    /// Register a sink that captures every dispatched mail's lineage
    /// triple into a shared `Vec`. Both lineage tests below share
    /// this setup; the helper returns the capture handle and the
    /// registered mailbox id.
    fn register_lineage_capture_sink(
        registry: &Arc<Registry>,
        name: &str,
    ) -> (LineageCapture, MailboxId) {
        let captured: LineageCapture = Arc::new(Mutex::new(Vec::new()));
        let captured_for_handler = Arc::clone(&captured);
        let sink_id = registry
            .try_register_inbox(
                name,
                Arc::new(move |dispatch: OwnedDispatch| {
                    // ADR-0094: terminal test capture sink — discharge.
                    dispatch.discharge();
                    captured_for_handler.lock().unwrap().push((
                        dispatch.mail_id,
                        dispatch.root,
                        dispatch.parent_mail,
                    ));
                }),
            )
            .expect("register sink");
        (captured, sink_id)
    }

    fn ctx() -> ComponentCtx {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(1024 * 1024));
        ComponentCtx::new(
            MailboxId(0),
            Arc::clone(&registry),
            Arc::new(Mailer::new(registry, store)),
            HubOutbound::disconnected(),
        )
    }

    fn instantiate(wat: &str) -> Component {
        let engine = Engine::default();
        let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
        host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(wat).expect("compile WAT");
        let module = Module::new(&engine, &wasm).expect("compile module");
        Component::instantiate(&engine, &linker, &module, ctx(), &[], None).expect("instantiate")
    }

    /// ADR-0090 helper: instantiate with explicit config bytes so a
    /// WAT-level `init_with_config_p32` can inspect the region the host placed
    /// the config in.
    fn instantiate_with_config(wat: &str, config_bytes: &[u8]) -> Component {
        try_instantiate_with_config(wat, config_bytes).expect("instantiate")
    }

    /// Non-panicking sibling of [`instantiate_with_config`] so the
    /// iamacoffeepot/aether#1390 rejection tests can assert the `Err` the
    /// substrate returns (which `dispatch_load_component` surfaces as
    /// `LoadResult::Err`) instead of unwrapping it.
    fn try_instantiate_with_config(wat: &str, config_bytes: &[u8]) -> wasmtime::Result<Component> {
        let engine = Engine::default();
        let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
        host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(wat).expect("compile WAT");
        let module = Module::new(&engine, &wasm).expect("compile module");
        Component::instantiate(&engine, &linker, &module, ctx(), config_bytes, None)
    }

    /// WAT where `on_dehydrate` writes 0x11 to offset 200 — same pattern
    /// as `control.rs` test shape but kept local so component tests
    /// stay standalone. (Issue 584 Phase 3 retired the legacy
    /// `on_drop` companion hook; pre-shutdown coverage rides
    /// [`WAT_WIRE_UNWIRE`] now.)
    const WAT_HOOKS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "on_dehydrate") (result i32)
                i32.const 200
                i32.const 0x11
                i32.store
                i32.const 0))
    "#;

    const WAT_NO_HOOKS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0))
    "#;

    /// ADR-0095: a minimal `realloc_p32` bump allocator for delivery test
    /// fixtures, interpolated into a fixture module via `format!`. Ignores
    /// `old_ptr` / `old_size` (leaks on grow — fine for tests), bump-allocates
    /// from page 1 (`0x10000`, clear of the low stamp offsets the fixtures use),
    /// grows linear memory to fit, and returns `0` for the free form
    /// (`new_size == 0`). Contains no `{`/`}`, so it interpolates cleanly.
    const WAT_REALLOC: &str = r#"
            (global $bump (mut i32) (i32.const 0x10000))
            (func (export "realloc_p32")
                (param $old_ptr i32) (param $old_size i32) (param $align i32) (param $new_size i32)
                (result i32)
                (local $ret i32)
                (local $end i32)
                (if (i32.eqz (local.get $new_size))
                    (then (return (i32.const 0))))
                (local.set $ret (global.get $bump))
                (local.set $end
                    (i32.and
                        (i32.add (i32.add (local.get $ret) (local.get $new_size)) (i32.const 7))
                        (i32.const -8)))
                (if (i32.gt_u (local.get $end) (i32.mul (memory.size) (i32.const 0x10000)))
                    (then
                        (drop (memory.grow
                            (i32.add
                                (i32.div_u
                                    (i32.sub (local.get $end) (i32.mul (memory.size) (i32.const 0x10000)))
                                    (i32.const 0x10000))
                                (i32.const 1))))))
                (global.set $bump (local.get $end))
                (local.get $ret))"#;

    /// ADR-0095: fixture whose `receive_p32` records the pointer it was handed
    /// at offset 16, so a test can prove a payload landed in the cached small
    /// region (`<= SMALL_REGION_BYTES`) or the grown large region. One page initially;
    /// the bump allocator grows memory for a large payload.
    fn wat_records_mail_ptr() -> String {
        format!(
            r#"
        (module
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 16
                local.get 1
                i32.store
                i32.const 0))
    "#
        )
    }

    /// ADR-0090 / ADR-0095: `init_with_config_p32` shim that stamps the host-
    /// provided `(mailbox_id, config_ptr, config_len)` triple at known offsets
    /// and copies the first two config bytes — so a test can assert which region
    /// the config landed in and that the bytes round-tripped. Exports
    /// `realloc_p32`, so config delivery routes through the allocator. Layout:
    ///
    ///   offset 200  : low 32 bits of `mailbox_id`
    ///   offset 204  : `config_ptr` (the small or grown delivery region)
    ///   offset 208  : `config_len`
    ///   offset 212  : first byte of config (when `config_len` >= 1)
    ///   offset 213  : second byte of config (when `config_len` >= 2)
    fn wat_init_with_config() -> String {
        format!(
            r#"
        (module
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "init_with_config_p32") (param i64 i32 i32) (result i32)
                ;; *(u32*)200 = low32(mailbox_id)
                i32.const 200
                local.get 0
                i32.wrap_i64
                i32.store
                ;; *(u32*)204 = config_ptr
                i32.const 204
                local.get 1
                i32.store
                ;; *(u32*)208 = config_len
                i32.const 208
                local.get 2
                i32.store
                ;; if config_len > 0, copy first byte to offset 212
                local.get 2
                i32.const 0
                i32.gt_u
                if
                    i32.const 212
                    local.get 1
                    i32.load8_u
                    i32.store8
                end
                ;; if config_len > 1, copy second byte to offset 213
                local.get 2
                i32.const 1
                i32.gt_u
                if
                    i32.const 213
                    local.get 1
                    i32.const 1
                    i32.add
                    i32.load8_u
                    i32.store8
                end
                i32.const 0))
    "#
        )
    }

    /// ADR-0095: a guest exporting `init_with_config_p32` but NO `realloc_p32`
    /// allocator. A config delivered to it can't be placed (the host owns no
    /// region in this guest), so instantiate returns a clean boot error rather
    /// than writing or trapping. Stamps the triple it never reaches.
    const WAT_INIT_CONFIG_NO_ALLOC: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "init_with_config_p32") (param i64 i32 i32) (result i32)
                i32.const 204
                local.get 1
                i32.store
                i32.const 208
                local.get 2
                i32.store
                i32.const 0))
    "#;

    /// WAT exercising the issue 584 Phase 2b lifecycle hooks. `wire`
    /// writes 0x77 to offset 100; `unwire` writes 0x88 to offset 104.
    /// Mailbox id arrives in the i64 param; we store its low 32 bits
    /// at offset 108 (wire) / 112 (unwire) so tests can verify the
    /// host passed the right value.
    const WAT_WIRE_UNWIRE: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "wire") (param i64) (result i32)
                i32.const 100
                i32.const 0x77
                i32.store
                i32.const 108
                local.get 0
                i32.wrap_i64
                i32.store
                i32.const 0)
            (func (export "unwire") (param i64) (result i32)
                i32.const 104
                i32.const 0x88
                i32.store
                i32.const 112
                local.get 0
                i32.wrap_i64
                i32.store
                i32.const 0))
    "#;

    /// WAT whose `wire` traps. Tests that `Component::instantiate`
    /// surfaces the trap as a wasmtime error rather than swallowing.
    const WAT_WIRE_TRAPS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "wire") (param i64) (result i32)
                unreachable))
    "#;

    /// WAT whose `unwire` traps. Tests that `Component::unwire`
    /// contains the trap (logs but doesn't propagate), same pattern
    /// as `on_dehydrate`'s trap-is-contained behaviour.
    const WAT_UNWIRE_TRAPS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "unwire") (param i64) (result i32)
                unreachable))
    "#;

    /// ADR-0016 save-side: `on_dehydrate` calls `save_state` with a
    /// version and 4 bytes at offset 300 (`0xDE 0xAD 0xBE 0xEF`).
    const WAT_SAVES_STATE: &str = r#"
        (module
            (import "aether" "save_state_p32"
                (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 300) "\de\ad\be\ef")
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "on_dehydrate") (result i32)
                (drop (call $save_state
                    (i32.const 7)    ;; version
                    (i32.const 300)  ;; ptr
                    (i32.const 4)))  ;; len
                i32.const 0))
    "#;

    /// ADR-0016 save-side: `on_dehydrate` attempts a save larger than
    /// the 1 MiB cap. The host fn records the error on the ctx and
    /// returns status 3 (too-large). The guest drops the return.
    const WAT_SAVES_TOO_LARGE: &str = r#"
        (module
            (import "aether" "save_state_p32"
                (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "on_dehydrate") (result i32)
                (drop (call $save_state
                    (i32.const 1)            ;; version
                    (i32.const 0)            ;; ptr
                    (i32.const 0x00200000))) ;; 2 MiB — over the cap
                i32.const 0))
    "#;

    /// ADR-0016 load-side: `on_rehydrate(version, ptr, len)` copies `len` bytes
    /// from `ptr` (the delivery region the host placed the state in) to offset
    /// 400 and writes `version` at offset 396. Exports `realloc_p32` so the
    /// state delivery has a region to land in. Bulk-memory (`memory.copy`) is on
    /// by default in wasmtime; no feature flag needed.
    fn wat_rehydrates() -> String {
        format!(
            r#"
        (module
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "on_rehydrate_p32") (param i32 i32 i32) (result i32)
                ;; *(u32*)396 = version
                i32.const 396
                local.get 0
                i32.store
                ;; memcpy(dst=400, src=ptr, n=len)
                i32.const 400
                local.get 1
                local.get 2
                memory.copy
                i32.const 0))
    "#
        )
    }

    /// ADR-0013: `receive` stores the sender handle at offset 500 so the test
    /// can observe what the substrate passed through. Exports `realloc_p32` so
    /// even an empty mail has a (non-null) region to be placed in.
    fn wat_stores_sender() -> String {
        format!(
            r#"
        (module
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 500
                local.get 4
                i32.store
                i32.const 0))
    "#
        )
    }

    /// ADR-0114 decision #1: `receive` stores the low 32 bits of the
    /// `recipient` param (the 6th, an `i64`) at offset 500 so the test
    /// can observe the routed mailbox the substrate threaded through.
    /// Exports `realloc_p32` so even an empty mail has a (non-null)
    /// region to be placed in.
    fn wat_stores_recipient() -> String {
        format!(
            r#"
        (module
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 500
                local.get 5
                i32.wrap_i64
                i32.store
                i32.const 0))
    "#
        )
    }

    /// ADR-0013: `receive` echoes a reply back to the sender under a
    /// caller-provided kind id. Payload is empty — the round-trip is
    /// the observable behavior. ADR-0030 Phase 2 made kind ids hashed,
    /// so the test builds the WAT with the live `kind_id_from_parts`
    /// for "test.pong" rather than a hardcoded sequential 0. Exports
    /// `realloc_p32` so the empty mail has a region to be placed in.
    fn wat_replies(kind_id: u64) -> String {
        format!(
            r#"
        (module
            (import "aether" "reply_mail_p32"
                (func $reply_mail (param i32 i64 i32 i32 i32 i64) (result i32)))
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                (drop (call $reply_mail
                    (local.get 4) ;; sender handle from receive param
                    (i64.const {kind_id}) ;; hashed kind id of "test.pong"
                    (i32.const 0) ;; ptr
                    (i32.const 0) ;; len
                    (i32.const 1) ;; count
                    (i64.const 0))) ;; from = NONE (issue 1987); falls back to self id
                i32.const 0))
        "#
        )
    }

    #[test]
    fn on_dehydrate_invokes_export_and_writes_marker() {
        let mut component = instantiate(WAT_HOOKS);
        assert_eq!(component.read_u32(200), 0);
        component.on_dehydrate();
        assert_eq!(component.read_u32(200), 0x11);
    }

    #[test]
    fn on_dehydrate_on_component_without_export_is_noop() {
        let mut component = instantiate(WAT_NO_HOOKS);
        // Just needs to not panic. No marker to check.
        component.on_dehydrate();
    }

    /// ADR-0090 / ADR-0095: `Component::instantiate` places `config_bytes` in a
    /// delivery region and calls `init_with_config_p32` with
    /// `(mailbox_id, config_ptr, len)`. A config that fits lands in the cached
    /// small region. The WAT shim stamps the triple at known offsets so this
    /// test can assert each leg without a real Kind decoder in scope.
    #[test]
    fn init_with_config_p32_threads_config_ptr_len_through() {
        let payload: &[u8] = &[0xAB, 0xCD, 0xEF, 0x12, 0x34];
        let mut component = instantiate_with_config(&wat_init_with_config(), payload);
        let small_ptr = component.small_ptr;
        // Mailbox id stamped: test ctx uses MailboxId(0), so low 32 bits are 0.
        assert_eq!(component.read_u32(200), 0);
        // config_ptr == the cached small region (fits under SMALL_REGION_BYTES).
        assert_eq!(component.read_u32(204), small_ptr);
        // config_len matches the slice the host wrote.
        let observed_len = component.read_u32(208);
        assert_eq!(observed_len as usize, payload.len());
        // The substrate physically wrote the bytes into the small region —
        // read them back through the host-side accessor.
        let observed = component.read_bytes(small_ptr as usize, payload.len());
        assert_eq!(observed, payload);
        // And the guest's shim could read the same bytes through
        // `(config_ptr + i)`; the two leading bytes copied via i32.load8_u
        // land at 212 + 213.
        assert_eq!(component.read_u32(212) & 0xFF, u32::from(payload[0]));
        assert_eq!(component.read_u32(213) & 0xFF, u32::from(payload[1]));
    }

    /// Companion: empty config (the trait-default `Config = ()` path) still
    /// calls `init_with_config_p32` with `(mailbox_id, small_ptr, 0)` — a
    /// non-null pointer (the cached small region) even with no bytes to write.
    #[test]
    fn init_with_config_p32_empty_config_passes_zero_length() {
        let mut component = instantiate_with_config(&wat_init_with_config(), &[]);
        let small_ptr = component.small_ptr;
        // Triple stamped, len == 0, config_ptr is the (non-null) small region.
        assert_eq!(component.read_u32(200), 0);
        assert_eq!(component.read_u32(204), small_ptr);
        assert_ne!(small_ptr, 0, "small region pointer must be non-null");
        assert_eq!(component.read_u32(208), 0);
        // No bytes were copied to 212 / 213 (the WAT skips the copy
        // when len == 0), so the slot stays zero.
        assert_eq!(component.read_u32(212), 0);
    }

    /// ADR-0095: a config at/under `SMALL_REGION_BYTES` lands in the cached small
    /// region, written directly with no allocator call.
    #[test]
    fn instantiate_small_config_uses_small_region() {
        let payload: &[u8] = &[0x01, 0x02, 0x03];
        let mut component = instantiate_with_config(&wat_init_with_config(), payload);
        let small_ptr = component.small_ptr;
        assert_eq!(component.read_u32(204), small_ptr);
        assert_eq!(component.read_u32(208) as usize, payload.len());
        assert_eq!(
            component.read_bytes(small_ptr as usize, payload.len()),
            payload,
        );
    }

    /// ADR-0095: a config larger than `SMALL_REGION_BYTES` but within the deliverable
    /// bound grows the large region — `init_with_config_p32` is handed the
    /// large-region pointer (not the small one) and the bytes round-trip to it.
    #[test]
    fn instantiate_large_config_grows_large_region() {
        // 900_000 > SMALL_REGION_BYTES (8 KiB), < MAX_DELIVERABLE_MAIL_BYTES.
        let mut payload = vec![0u8; 900_000];
        payload[0] = 0xA1;
        payload[1] = 0xB2;
        let mut component = instantiate_with_config(&wat_init_with_config(), &payload);
        let large_ptr = component.large_ptr;
        assert_ne!(large_ptr, 0, "large region must have been grown");
        assert_ne!(
            large_ptr, component.small_ptr,
            "large config must not land in the small region",
        );
        // init saw the large-region pointer.
        assert_eq!(component.read_u32(204), large_ptr);
        assert_eq!(component.read_u32(208) as usize, payload.len());
        // The substrate physically wrote the config at the large pointer.
        assert_eq!(component.read_bytes(large_ptr as usize, 4), payload[..4]);
        // The guest's shim read the first two bytes back through (config_ptr + i).
        assert_eq!(component.read_u32(212) & 0xFF, u32::from(payload[0]));
        assert_eq!(component.read_u32(213) & 0xFF, u32::from(payload[1]));
    }

    /// ADR-0095: a config past the absolute ceiling is a clean boot error —
    /// `instantiate` returns `Err` (→ `LoadResult::Err`) without writing or
    /// trapping. The guard fires on the length check before any allocator call.
    #[test]
    fn instantiate_oversize_config_returns_clean_error() {
        let payload = vec![0u8; MAX_DELIVERABLE_MAIL_BYTES + 1];
        // `Component` is not `Debug`, so match rather than `expect_err`.
        let Err(err) = try_instantiate_with_config(&wat_init_with_config(), &payload) else {
            panic!("oversize config must fail to instantiate");
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("exceeds") && msg.contains("deliverable"),
            "error should name the deliverable bound; got: {msg}",
        );
    }

    /// ADR-0095: a config delivered to a guest with NO `realloc_p32` allocator is
    /// a clean boot error, not a trap — the host owns no region in that guest, so
    /// the guard fires before any write.
    #[test]
    fn instantiate_config_without_allocator_returns_clean_error() {
        let payload: &[u8] = &[0x01, 0x02, 0x03];
        // `Component` is not `Debug`, so match rather than `expect_err`.
        let Err(err) = try_instantiate_with_config(WAT_INIT_CONFIG_NO_ALLOC, payload) else {
            panic!("config to a guest without an allocator must fail");
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("no realloc_p32 allocator"),
            "error should name the missing allocator; got: {msg}",
        );
    }

    #[test]
    fn on_dehydrate_save_state_populates_bundle() {
        let mut component = instantiate(WAT_SAVES_STATE);
        assert!(component.take_saved_state().is_none());
        component.on_dehydrate();
        let bundle = component.take_saved_state().expect("bundle saved");
        assert_eq!(bundle.version, 7);
        assert_eq!(bundle.bytes, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        // take_saved_state is destructive.
        assert!(component.take_saved_state().is_none());
    }

    /// Issue 584 Phase 2b: `Component::wire` invokes the guest's
    /// `wire` export. Issue 640 Phase 2 moved the call out of
    /// `instantiate` (which runs in `spawn_actor` step 4 — before
    /// the trampoline mailbox is registered) into the trampoline's
    /// `NativeActor::wire` body (post-registration), so wire-time
    /// `subscribe_input` mail validates against a live closure
    /// entry rather than racing the input cap's
    /// `validate_subscriber_mailbox`. The fixture writes 0x77 to
    /// offset 100 from inside its `wire` export; reading it back
    /// after `Component::wire()` proves the call dispatched.
    #[test]
    fn wire_invokes_export_and_writes_marker() {
        let mut component = instantiate(WAT_WIRE_UNWIRE);
        // wire hasn't been invoked yet — `instantiate` no longer fires it.
        assert_eq!(component.read_u32(100), 0);
        component.wire().expect("wire ok");
        assert_eq!(
            component.read_u32(100),
            0x77,
            "wire must run when Component::wire is invoked",
        );
        // Mailbox id stamped into offset 108 by the WAT — test ctx
        // uses MailboxId(0), so the low 32 bits are 0.
        assert_eq!(component.read_u32(108), 0);
    }

    /// Issue 584 Phase 2b: `Component::unwire` invokes the guest's
    /// `unwire` export. Trampoline calls this before `on_dehydrate`
    /// on the dying instance, or before the `Component` value drops
    /// on a `DropComponent`.
    #[test]
    fn unwire_invokes_export_and_writes_marker() {
        let mut component = instantiate(WAT_WIRE_UNWIRE);
        assert_eq!(component.read_u32(104), 0);
        component.unwire();
        assert_eq!(component.read_u32(104), 0x88);
    }

    /// Issue 584 Phase 2b / Issue 640 Phase 2: a wire trap is
    /// fatal — `Component::wire` returns the wasmtime error so the
    /// trampoline can log it. Pre-issue-640 the wire call lived
    /// inside `Component::instantiate`, so a wire trap aborted load
    /// directly; post-issue-640 it lives on the trampoline's
    /// `NativeActor::wire` lifecycle hook, so the trap surfaces
    /// after instantiation succeeds and the trampoline logs +
    /// continues (matching `unwire`'s contained-trap policy).
    #[test]
    fn wire_trap_propagates_via_component_wire() {
        let mut component = instantiate(WAT_WIRE_TRAPS);
        let result = component.wire();
        assert!(
            result.is_err(),
            "Component::wire must propagate the guest trap as wasmtime::Error",
        );
    }

    /// Issue 584 Phase 2b: `unwire` traps are contained the same way
    /// `on_dehydrate` traps are — logged but not propagated (per
    /// ADR-0015, panicking hooks must not stall teardown).
    #[test]
    fn unwire_trap_is_contained() {
        let mut component = instantiate(WAT_UNWIRE_TRAPS);
        // `unreachable` traps; substrate logs and continues. Reaching
        // the line after the call is the whole assertion.
        component.unwire();
    }

    /// Issue 584 Phase 2b: a component without a `wire` / `unwire`
    /// export is a no-op (matches the `on_dehydrate` pattern).
    #[test]
    fn unwire_on_component_without_export_is_noop() {
        let mut component = instantiate(WAT_NO_HOOKS);
        component.unwire();
    }

    /// ADR-0095: a payload at/under `SMALL_REGION_BYTES` lands in the cached small
    /// region — `receive` runs (rc 0) and is handed the small region pointer.
    #[test]
    fn deliver_small_payload_uses_small_region() {
        let mut component = instantiate(&wat_records_mail_ptr());
        let small_ptr = component.small_ptr;
        // 100 bytes <= SMALL_REGION_BYTES (8 KiB).
        let mail = Mail::new(MailboxId(0), aether_data::KindId(0), vec![0u8; 100], 1);
        let rc = component.deliver(&mail).expect("deliver ok");
        assert_eq!(rc, 0, "guest receive should have run");
        // The fixture's `receive` recorded the pointer it was handed at offset 16.
        assert_eq!(
            component.read_u32(16),
            small_ptr,
            "small payload should land in the cached small region",
        );
    }

    /// ADR-0095: a payload larger than `SMALL_REGION_BYTES` but within the deliverable
    /// bound grows the large region — `receive` runs (rc 0) and is handed the
    /// large-region pointer, not the small one.
    #[test]
    fn deliver_large_payload_grows_large_region() {
        let mut component = instantiate(&wat_records_mail_ptr());
        // 900_000 > SMALL_REGION_BYTES (8 KiB), < MAX_DELIVERABLE_MAIL_BYTES.
        let mail = Mail::new(MailboxId(0), aether_data::KindId(0), vec![0u8; 900_000], 1);
        let rc = component.deliver(&mail).expect("deliver ok");
        assert_eq!(rc, 0, "guest receive should have run");
        let large_ptr = component.large_ptr;
        assert_ne!(large_ptr, 0, "large region must have been grown");
        assert_ne!(large_ptr, component.small_ptr);
        assert_eq!(
            component.read_u32(16),
            large_ptr,
            "large payload should land in the grown large region",
        );
    }

    /// ADR-0095: a payload past the absolute deliverable ceiling is dropped
    /// cleanly (no trap, no write) even when the guest exports an allocator —
    /// the guard fires on the length check.
    #[test]
    fn deliver_oversize_payload_dropped() {
        let mut component = instantiate(&wat_records_mail_ptr());
        let mail = Mail::new(
            MailboxId(0),
            aether_data::KindId(0),
            vec![0u8; MAX_DELIVERABLE_MAIL_BYTES + 1],
            1,
        );
        let rc = component.deliver(&mail).expect("deliver must not trap");
        assert_eq!(rc, DISPATCH_DROPPED_OVERSIZE);
    }

    /// ADR-0095: a guest that exports no `realloc_p32` allocator can't receive
    /// any payload — the host owns no region in it, so delivery drops cleanly
    /// rather than trapping on a write.
    #[test]
    fn deliver_to_guest_without_allocator_dropped() {
        let mut component = instantiate(WAT_NO_HOOKS);
        let mail = Mail::new(MailboxId(0), aether_data::KindId(0), vec![0u8; 64], 1);
        let rc = component.deliver(&mail).expect("deliver must not trap");
        assert_eq!(rc, DISPATCH_DROPPED_OVERSIZE);
    }

    #[test]
    fn on_dehydrate_save_state_without_export_leaves_bundle_empty() {
        let mut component = instantiate(WAT_NO_HOOKS);
        component.on_dehydrate();
        assert!(component.take_saved_state().is_none());
        assert!(component.take_save_error().is_none());
    }

    #[test]
    fn save_state_over_cap_records_error_and_no_bundle() {
        let mut component = instantiate(WAT_SAVES_TOO_LARGE);
        component.on_dehydrate();
        let err = component.take_save_error().expect("error recorded");
        assert!(err.contains("exceeds"), "got: {err}");
        assert!(component.take_saved_state().is_none());
    }

    #[test]
    fn call_on_rehydrate_writes_bytes_and_invokes_hook() {
        let mut component = instantiate(&wat_rehydrates());
        let bundle = StateBundle {
            version: 0x2A,
            bytes: vec![0x01, 0x02, 0x03, 0x04, 0x05],
        };
        component.call_on_rehydrate(&bundle).expect("rehydrate ok");
        // Hook copied the version to offset 396 and the bytes to 400.
        assert_eq!(component.read_u32(396), 0x2A);
        assert_eq!(
            component.read_bytes(400, 5),
            vec![0x01, 0x02, 0x03, 0x04, 0x05],
        );
    }

    #[test]
    fn call_on_rehydrate_without_export_is_noop() {
        let mut component = instantiate(WAT_NO_HOOKS);
        let bundle = StateBundle {
            version: 1,
            bytes: vec![9, 9, 9],
        };
        // Silently discards the bundle per ADR-0016 §3.
        component.call_on_rehydrate(&bundle).expect("noop ok");
    }

    #[test]
    fn deliver_with_nil_sender_passes_sender_none() {
        use crate::actor::wasm::reply_table::NO_REPLY_HANDLE;
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};

        let mut component = instantiate(&wat_stores_sender());
        // Mail::new defaults sender to SessionToken::NIL.
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1);
        component.deliver(&mail).expect("deliver");
        assert_eq!(component.read_u32(500), NO_REPLY_HANDLE);
    }

    /// ADR-0114 decision #1 end-to-end through the dispatch unit path:
    /// `Component::deliver` reads the routed `mail.recipient` and threads
    /// it as the trailing `receive_p32` frame slot, so the guest reads
    /// the address its mail was sent to. The production trampoline routes
    /// a normally-addressed actor's mail with `recipient == self.mailbox`
    /// (the actor's own id), so the guest sees its own mailbox id; this
    /// unit fixture sends a distinct recipient to prove the value the
    /// substrate routes is exactly what the guest receives.
    #[test]
    fn deliver_threads_recipient_to_guest() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};

        let mut component = instantiate(&wat_stores_recipient());
        // A recipient whose low 32 bits are observable through the WAT
        // fixture's `i32.wrap_i64` store. The high bits (tag nibble +
        // hash) are dropped by the wrap — the low word is enough to
        // prove the routed id, not the reply handle, reached the guest.
        let recipient = M(0x9999_0000_1234_5678);
        let mail = SubstrateMail::new(recipient, aether_data::KindId(0), vec![], 1);
        component.deliver(&mail).expect("deliver");
        assert_eq!(
            component.read_u32(500),
            0x1234_5678,
            "guest must receive the routed recipient's low word as the 6th receive param",
        );
    }

    #[test]
    fn deliver_with_real_token_allocates_session_handle() {
        use crate::actor::wasm::reply_table::{NO_REPLY_HANDLE, ReplyEntry};
        use crate::mail::{Mail as SubstrateMail, MailboxId as M, Source, SourceAddr};
        use aether_data::{SessionToken, Uuid};

        let mut component = instantiate(&wat_stores_sender());
        let token = SessionToken(Uuid::from_u128(0xaaaa));
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
            .with_reply_to(Source::to(SourceAddr::Session(token)));
        component.deliver(&mail).expect("deliver");
        let observed = component.read_u32(500);
        assert_ne!(observed, NO_REPLY_HANDLE);
        assert_eq!(
            component.store.data().reply_table.resolve(observed),
            Some(ReplyEntry::session(token)),
        );
    }

    #[test]
    fn deliver_with_component_reply_target_allocates_component_handle() {
        use crate::actor::wasm::reply_table::{NO_REPLY_HANDLE, ReplyEntry};
        use crate::mail::{Mail as SubstrateMail, MailboxId as M, Source, SourceAddr};

        let mut component = instantiate(&wat_stores_sender());
        // ADR-0017 / issue #644: component-origin mail (peer-to-peer
        // send sets `reply_to.addr = Component(sender)`) gets a
        // Component-variant handle.
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
            .with_reply_to(Source::to(SourceAddr::Component(M(7))));
        component.deliver(&mail).expect("deliver");
        let observed = component.read_u32(500);
        assert_ne!(observed, NO_REPLY_HANDLE);
        assert_eq!(
            component.store.data().reply_table.resolve(observed),
            Some(ReplyEntry::component(M(7))),
        );
    }

    /// Issue 2001: `receive` stores the low 32 bits of the `source` param
    /// (the 7th, an `i64`) at offset 500 so the test can observe the
    /// resolved inbound source the substrate threaded through. Mirrors
    /// [`wat_stores_recipient`]. Exports `realloc_p32` so even an empty
    /// mail has a (non-null) region to be placed in.
    fn wat_stores_source() -> String {
        format!(
            r#"
        (module
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 500
                local.get 6
                i32.wrap_i64
                i32.store
                i32.const 0))
        "#
        )
    }

    /// Issue 2001 end-to-end through the dispatch unit path: `deliver`
    /// resolves the inbound `SourceAddr` and threads it as the trailing
    /// `receive_p32` slot. A peer-component origin yields that mailbox's
    /// raw id; a session / engine / no-reply origin yields `0`
    /// (`MailboxId::NONE`) — the same contract `source_of_p32` had.
    #[test]
    fn deliver_threads_component_source_to_guest() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M, Source, SourceAddr};

        let mut component = instantiate(&wat_stores_source());
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
            .with_reply_to(Source::to(SourceAddr::Component(M(0x9999_0000_1234_5678))));
        component.deliver(&mail).expect("deliver");
        assert_eq!(
            component.read_u32(500),
            0x1234_5678,
            "guest must receive the peer-component source's low word as the 7th receive param",
        );
    }

    #[test]
    fn deliver_threads_zero_source_for_session_origin() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M, Source, SourceAddr};
        use aether_data::{SessionToken, Uuid};

        let mut component = instantiate(&wat_stores_source());
        let token = SessionToken(Uuid::from_u128(0xdead));
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
            .with_reply_to(Source::to(SourceAddr::Session(token)));
        component.deliver(&mail).expect("deliver");
        assert_eq!(
            component.read_u32(500),
            0,
            "a session origin must thread 0 (MailboxId::NONE) as the source param",
        );
    }

    #[test]
    fn deliver_threads_zero_source_for_no_reply_target() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};

        let mut component = instantiate(&wat_stores_source());
        // No reply target → SourceAddr::None → source param is 0. The guest's
        // store overwrites offset 500 with the threaded 0 regardless of any
        // prior value, proving the substrate threaded NONE.
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1);
        component.deliver(&mail).expect("deliver");
        assert_eq!(
            component.read_u32(500),
            0,
            "a no-reply-target origin must thread 0 as the source param",
        );
    }

    fn plane_ctx_for_reply() -> (ComponentCtx, Receiver<EgressEvent>, aether_data::KindId) {
        use crate::mail::MailboxId as M;
        use aether_data::{KindDescriptor, SchemaType};

        let (outbound, rx) = HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let pong_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "test.pong".into(),
                schema: SchemaType::Unit,
            })
            .expect("register kind");
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let ctx = ComponentCtx::new(M(0), registry, mailer, outbound);
        (ctx, rx, pong_id)
    }

    fn instantiate_with_ctx(wat: &str, ctx: ComponentCtx) -> Component {
        let engine = Engine::default();
        let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
        host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(wat).unwrap();
        let module = Module::new(&engine, &wasm).unwrap();
        Component::instantiate(&engine, &linker, &module, ctx, &[], None).unwrap()
    }

    #[test]
    fn reply_mail_emits_session_addressed_frame() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M, Source, SourceAddr};
        use aether_data::{SessionToken, Uuid};

        let (ctx, rx, pong_id) = plane_ctx_for_reply();
        let mut component = instantiate_with_ctx(&wat_replies(pong_id.0), ctx);

        let token = SessionToken(Uuid::from_u128(0xbeef));
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
            .with_reply_to(Source::to(SourceAddr::Session(token)));
        component.deliver(&mail).expect("deliver");

        let event = rx.try_recv().expect("outbound egress queued");
        let EgressEvent::ToSession {
            session, kind_name, ..
        } = event
        else {
            panic!("expected ToSession egress, got {event:?}");
        };
        assert_eq!(session, token);
        assert_eq!(kind_name, "test.pong");
    }

    #[test]
    fn reply_mail_with_unknown_handle_sends_no_frame() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};

        let (ctx, rx, pong_id) = plane_ctx_for_reply();
        let mut component = instantiate_with_ctx(&wat_replies(pong_id.0), ctx);

        // NIL sender → NO_REPLY_HANDLE reaches the guest → reply_mail
        // returns REPLY_UNKNOWN_HANDLE and outbound stays quiet.
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1);
        component.deliver(&mail).expect("deliver");
        assert!(rx.try_recv().is_err(), "no frame should have been sent");
    }

    /// Issue iamacoffeepot/aether#1465: a wasm component that replies to
    /// an inbound whose reply target is `SourceAddr::Component` must
    /// echo the inbound `correlation` on the outgoing reply, with
    /// reply-of-a-reply target `None` — the ADR-0042 contract the
    /// `Session` / `EngineMailbox` arms and native `Mailer::send_reply`
    /// already honor. Before the fix the Component arm routed through
    /// `ComponentCtx::send`, which fresh-minted a `Component(self)`
    /// correlation, so the reply arrived with the wrong correlation and
    /// target and the RPC server (matching by correlation against its
    /// `in_flight` table) dropped it. Because the inbound target is a
    /// peer `Component`, this also exercises the ADR-0042 component→
    /// component reply-correlation path by construction. Drives the
    /// `reply_mail_p32` Component arm through a guest and asserts the
    /// dispatched reply's `Source`.
    #[test]
    fn reply_mail_component_target_echoes_inbound_correlation() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M, Source, SourceAddr};
        use aether_data::{KindDescriptor, SchemaType};

        // A non-trivial inbound correlation that can't be mistaken for a
        // fresh `mint_correlation` value (which would start at `1`).
        const INBOUND_CORRELATION: u64 = 0x5151;

        let registry = Arc::new(Registry::new());
        // The reply recipient: a capture inbox that records the
        // dispatched mail's `Source` (the reply's `sender`).
        let captured: Arc<Mutex<Vec<Source>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_handler = Arc::clone(&captured);
        let recipient = registry
            .try_register_inbox(
                "issue_1465_reply_recipient",
                Arc::new(move |dispatch: OwnedDispatch| {
                    // ADR-0094: terminal test capture sink — discharge.
                    dispatch.discharge();
                    captured_for_handler.lock().unwrap().push(dispatch.sender);
                }),
            )
            .expect("register reply recipient");
        // The reply kind must be known so the Component arm's validation
        // guard (`kind_name(kind).is_some()`) passes.
        let pong_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "test.pong".into(),
                schema: SchemaType::Unit,
            })
            .expect("register kind");

        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let ctx = ComponentCtx::new(
            M(0),
            Arc::clone(&registry),
            mailer,
            HubOutbound::disconnected(),
        );
        let mut component = instantiate_with_ctx(&wat_replies(pong_id.0), ctx);

        // Inbound whose reply target is a peer component, carrying
        // `INBOUND_CORRELATION`. The guest's `receive_p32` calls
        // `reply_mail_p32` with the sender handle the substrate
        // allocated for this reply target.
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1).with_reply_to(
            Source::with_correlation(SourceAddr::Component(recipient), INBOUND_CORRELATION),
        );
        component.deliver(&mail).expect("deliver");

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "reply should have dispatched once");
        let reply_to = captured[0];
        assert_eq!(
            reply_to.correlation_id, INBOUND_CORRELATION,
            "reply must echo the inbound correlation, not a fresh mint",
        );
        assert_eq!(
            reply_to.addr,
            SourceAddr::None,
            "reply-of-a-reply target must be None, matching native send_reply",
        );
    }

    /// ADR-0037 Phase 1 + Phase 2: when a component sends to a mailbox
    /// id the local registry doesn't know, `ctx.send` defers to the
    /// mailer, which emits an upstream `MailToHubSubstrate` frame
    /// carrying the sender's mailbox id so the hub can build a
    /// `Source::EngineMailbox` for the receiving component.
    #[test]
    fn unknown_recipient_bubbles_up_with_sender_mailbox() {
        let (outbound, outbound_rx) = HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let sender = registry
            .try_register_inbox("client", registry::noop_handler())
            .expect("register client mailbox");

        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(
            Mailer::new(Arc::clone(&registry), store).with_outbound(Arc::clone(&outbound)),
        );

        let ctx = ComponentCtx::new(sender, Arc::clone(&registry), Arc::clone(&mailer), outbound);

        let unknown = MailboxId(0xDEAD_BEEF_u64);
        let kind = aether_data::KindId(0xABCD_u64);
        // `from = NONE` → the dispatch identity falls back to `self.sender`.
        ctx.send(unknown, kind, vec![1, 2, 3], 1, MailboxId::NONE);

        let event = outbound_rx.try_recv().expect("bubble-up event emitted");
        match event {
            EgressEvent::UnresolvedMail {
                recipient_mailbox_id,
                kind_id,
                payload,
                count,
                source_mailbox_id,
                ..
            } => {
                assert_eq!(recipient_mailbox_id, unknown);
                assert_eq!(kind_id, kind);
                assert_eq!(payload, vec![1, 2, 3]);
                assert_eq!(count, 1);
                assert_eq!(source_mailbox_id, Some(sender));
            }
            other => panic!("expected UnresolvedMail egress, got {other:?}"),
        }
    }

    /// No hub wired (disconnected substrate, or the hub chassis
    /// itself): unknown recipients still warn-drop — no crash, no
    /// upstream frame.
    #[test]
    fn unknown_recipient_without_outbound_warn_drops() {
        let (outbound, outbound_rx) = HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let sender = registry
            .try_register_inbox("client", registry::noop_handler())
            .expect("register client mailbox");

        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        // Deliberately no `with_outbound` — exercises the local warn-drop path.

        let ctx = ComponentCtx::new(sender, Arc::clone(&registry), Arc::clone(&mailer), outbound);

        ctx.send(
            MailboxId(0xDEAD_BEEF_u64),
            aether_data::KindId(0xABCD),
            vec![],
            0,
            MailboxId::NONE,
        );
        assert!(
            outbound_rx.try_recv().is_err(),
            "no bubble-up without a wired outbound"
        );
    }

    /// Issue iamacoffeepot/aether#722: when `Component::deliver` populates
    /// `ComponentCtx::set_in_flight`, any subsequent `ctx.send` stamps
    /// `parent_mail = Some(in_flight_mail_id)` and inherits the chain
    /// `root` — closing the wasm-side gap that previously orphaned every
    /// guest-triggered send. This test exercises the closure-handler
    /// branch: register a sink that captures the inbound `MailDispatch`
    /// fields, set in-flight on the ctx, send to the sink, and assert
    /// the captured lineage matches.
    #[test]
    fn send_propagates_in_flight_lineage_on_closure_branch() {
        let registry = Arc::new(Registry::new());
        let (captured, sink_id) = register_lineage_capture_sink(&registry, "issue_722_sink");

        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x42));
        let ctx = ComponentCtx::new(
            sender,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            HubOutbound::disconnected(),
        );

        // Inbound lineage: the chassis-driven tick chain we're "in"
        // when the wasm guest's on_tick handler fires its outbound.
        let inbound_root = MailId::new(MailboxId::CHASSIS_MAILBOX_ID, 7);
        let inbound_mail = MailId::new(MailboxId(aether_data::with_tag(Tag::Mailbox, 0x99)), 42);
        ctx.set_in_flight(inbound_mail, inbound_root);

        ctx.send(
            sink_id,
            aether_data::KindId(0xABCD),
            vec![1, 2, 3],
            1,
            MailboxId::NONE,
        );

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "sink should have been called once");
        let (mail_id, root, parent) = captured[0];
        assert_eq!(
            parent,
            Some(inbound_mail),
            "parent_mail must point at inbound"
        );
        assert_eq!(root, inbound_root, "root must inherit from inbound chain");
        // The minted mail_id is fresh — sender = self, correlation
        // from the per-component counter (starts at 1 for the first send).
        assert_eq!(mail_id.sender, sender);
        assert_ne!(mail_id, inbound_mail, "outbound mail_id must be fresh");
    }

    /// Companion: with no in-flight context (chassis-bypass / test
    /// fixture), `ctx.send` mints a fresh root chain — `parent_mail`
    /// is `None` and `root == mail_id`. This is the same shape
    /// `NativeBinding::send_mail_with_lineage(None, None)` produces.
    #[test]
    fn send_without_in_flight_mints_fresh_root_chain() {
        let registry = Arc::new(Registry::new());
        let (captured, sink_id) =
            register_lineage_capture_sink(&registry, "issue_722_fresh_root_sink");

        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x33));
        let ctx = ComponentCtx::new(
            sender,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            HubOutbound::disconnected(),
        );
        // No `set_in_flight` call.

        ctx.send(
            sink_id,
            aether_data::KindId(0xCAFE),
            vec![],
            1,
            MailboxId::NONE,
        );

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let (mail_id, root, parent) = captured[0];
        assert!(parent.is_none(), "no inbound -> no parent edge");
        assert_eq!(root, mail_id, "fresh chain: root == mail_id");
        assert_eq!(mail_id.sender, sender);
    }

    /// ADR-0080 §7 (issue 1802): even with an in-flight inbound chain
    /// set, `ComponentCtx::send_detached` (the guest's `send_detached`,
    /// routed by the `send_mail_p32` host fn when its detached flag is
    /// set) ignores the lineage and opens a fresh chain — `parent_mail`
    /// is `None` and `root == mail_id`, the same shape as a no-inbound
    /// send. This is the wasm-side opt-out that mirrors the native
    /// `NativeActorMailbox::send_detached`.
    #[test]
    fn send_detached_mints_fresh_chain_despite_in_flight() {
        let registry = Arc::new(Registry::new());
        let (captured, sink_id) =
            register_lineage_capture_sink(&registry, "issue_1802_detached_sink");

        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x55));
        let ctx = ComponentCtx::new(
            sender,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            HubOutbound::disconnected(),
        );

        // Set an in-flight chain the default `send` would inherit.
        let inbound_root = MailId::new(MailboxId::CHASSIS_MAILBOX_ID, 9);
        let inbound_mail = MailId::new(MailboxId(aether_data::with_tag(Tag::Mailbox, 0x77)), 13);
        ctx.set_in_flight(inbound_mail, inbound_root);

        ctx.send_detached(
            sink_id,
            aether_data::KindId(0xF00D),
            vec![7, 8],
            1,
            MailboxId::NONE,
        );

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "sink should have been called once");
        let (mail_id, root, parent) = captured[0];
        assert!(
            parent.is_none(),
            "detached send carries no parent edge despite in-flight"
        );
        assert_eq!(root, mail_id, "detached send is its own root");
        assert_eq!(mail_id.sender, sender);
    }

    /// ADR-0114 step 1: the inline-child alias id the `spawn_inline_child`
    /// host fn folds — `with_tag(Mailbox, fold_lineage(parent_carry,
    /// instanced(aether.embedded, subname)))` — equals the parse → fold of
    /// the rendered lineage name (`mailbox_id_from_path`), so a wire `Call`
    /// addressing the child by name resolves to the same id the guest
    /// keys its membrane on (the post-#1920 convention). The parent carry
    /// mirrors a depth-2 loaded component (`aether.component/aether.embedded:NAME`).
    #[test]
    fn inline_alias_folded_id_matches_post_1920_convention() {
        let parent_carry = aether_data::fold_lineage(
            aether_data::ActorId::singleton("aether.component").0,
            aether_data::ActorId::instanced("aether.embedded", "testparent"),
        );
        let folded = MailboxId(aether_data::with_tag(
            Tag::Mailbox,
            aether_data::fold_lineage(
                parent_carry,
                aether_data::ActorId::instanced(TRAMPOLINE_NAMESPACE, "widget"),
            ),
        ));
        let from_path = aether_data::mailbox_id_from_path(
            "aether.component/aether.embedded:testparent/aether.embedded:widget",
        );
        assert_eq!(
            folded, from_path,
            "the host-fn alias fold matches the rendered-name parse → fold",
        );
    }

    /// ADR-0114 step 1: an alias `MailboxEntry` cloned from the parent's
    /// `Inbox` routes mail addressed to the alias into the parent slot's
    /// inbox, and the rendered alias name resolves (the engine's `Call`
    /// recipient-name path) to the alias id. Mirrors the `spawn_inline_child`
    /// host fn's registration against a depth-2 parent registered with its
    /// lineage-folded id.
    #[test]
    fn inline_alias_routes_into_parent_slot_inbox() {
        let registry = Arc::new(Registry::new());
        let captured: LineageCapture = Arc::new(Mutex::new(Vec::new()));
        let captured_for_handler = Arc::clone(&captured);
        let parent_name = "aether.component/aether.embedded:testparent".to_owned();
        let parent_id = aether_data::mailbox_id_from_path(&parent_name);
        registry
            .try_register_inbox_with_id(
                parent_id,
                parent_name.clone(),
                Arc::new(move |dispatch: OwnedDispatch| {
                    dispatch.discharge();
                    captured_for_handler.lock().unwrap().push((
                        dispatch.mail_id,
                        dispatch.root,
                        dispatch.parent_mail,
                    ));
                }),
            )
            .expect("parent registers under its lineage id");

        // Mirror the host fn: fold the alias id and register an alias route
        // to the parent's slot by cloning the parent's Inbox handler.
        let alias_name = format!("{parent_name}/aether.embedded:widget");
        let alias_id = aether_data::mailbox_id_from_path(&alias_name);
        let Some(MailboxEntry::Inbox { handler, .. }) = registry.entry(parent_id) else {
            panic!("parent is registered as a live Inbox");
        };
        registry
            .try_register_inbox_with_id(alias_id, alias_name.clone(), handler)
            .expect("alias registers under the folded id");

        // Name resolution (the wire `Call` path) resolves the alias.
        assert_eq!(
            registry.lookup(&alias_name),
            Some(alias_id),
            "the rendered alias name resolves to the folded alias id",
        );

        // Mail addressed to the alias lands in the parent slot's inbox.
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Mailer::new(Arc::clone(&registry), store);
        mailer.push(Mail::new(
            alias_id,
            aether_data::KindId(0xABCD),
            vec![1, 2, 3],
            1,
        ));
        assert_eq!(
            captured.lock().unwrap().len(),
            1,
            "alias mail dispatched into the parent slot's inbox",
        );
    }

    /// Issue 1987: a guest `send` carrying `from == self` (a
    /// normally-addressed actor) stamps the component as origin — the no-op
    /// regression that guards a normally-addressed actor.
    #[test]
    fn send_stamps_self_when_recipient_is_own_mailbox() {
        let registry = Arc::new(Registry::new());
        let (captured, sink_id) =
            register_lineage_capture_sink(&registry, "inline_self_origin_sink");
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x42));
        let ctx = ComponentCtx::new(
            sender,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            HubOutbound::disconnected(),
        );

        // `from == self` (a normally-addressed actor).
        ctx.send(sink_id, aether_data::KindId(0xABCD), vec![], 1, sender);

        let captured = captured.lock().unwrap();
        let (mail_id, _root, _parent) = captured[0];
        assert_eq!(
            mail_id.sender, sender,
            "origin stamps the component's own id when from == self",
        );
    }

    /// Issue 1987: a guest `send` carrying `from == an inline-child alias`
    /// stamps the alias as origin — the guest-carried `from` becomes the
    /// dispatch identity, so the child's sends carry the child's address.
    #[test]
    fn send_stamps_alias_when_recipient_is_inline_child() {
        let registry = Arc::new(Registry::new());
        let (captured, sink_id) =
            register_lineage_capture_sink(&registry, "inline_alias_origin_sink");
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x42));
        let alias = MailboxId(aether_data::with_tag(Tag::Mailbox, 0xA11A5));
        let ctx = ComponentCtx::new(
            sender,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            HubOutbound::disconnected(),
        );

        // `from == an inline-child alias` distinct from the component's own id.
        ctx.send(sink_id, aether_data::KindId(0xABCD), vec![], 1, alias);

        let captured = captured.lock().unwrap();
        let (mail_id, _root, _parent) = captured[0];
        assert_eq!(
            mail_id.sender, alias,
            "origin stamps the alias (dispatch identity) when from is a child",
        );
        assert_ne!(
            mail_id.sender, sender,
            "the child's send must not stamp the parent component",
        );
    }
}
