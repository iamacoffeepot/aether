use std::cell::Cell;
use std::mem;
use std::sync::Arc;

use crate::actor::native::binding::NativeBinding;
use crate::actor::wasm::reply_table::ReplyTable;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{MailboxEntry, OwnedDispatch, PreparedAliasRoute, Registry};
use crate::mail::{Mail, MailId, MailKind, MailboxId, Source, SourceAddr};
use crate::scheduler::pending_depth;

use crate::actor::wasm::asset_manifest::LoadWindow;

use super::StateBundle;

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
    /// returns `Err(ActorInitError)`. Issue 525 Phase 4b / issue 531: the
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
    /// `WasmTrampoline::init` (in `aether-component`; issue 634
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
    /// Current inbound reply correlation exposed through
    /// `reply_correlation_p32`. Set only for reply envelopes
    /// (`SourceAddr::None` plus a non-zero correlation) during
    /// [`super::Component::deliver`], then cleared after the guest returns.
    reply_correlation: Cell<u64>,
    /// ADR-0080 §5 in-flight inbound `MailId`. Set by
    /// [`super::Component::deliver`] before invoking the guest's
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
    /// ADR-0097: sibling-spawn requests staged by the `spawn_sibling`
    /// host fn and drained by the trampoline after `receive_p32`
    /// returns — the same host-fn-stages / host-drains pattern as
    /// `saved_state`. Empty outside an in-flight spawn; a handler that
    /// calls `spawn_child` more than once in one `receive` stages one
    /// entry per call, in guest call order. The trampoline performs the
    /// actual `spawn_child::<WasmTrampoline>`; substrate can't name that
    /// capabilities-layer type (ADR-0097 §4).
    pub pending_spawns: Vec<PendingSpawn>,
    /// ADR-0165: logical inline-child aliases staged by the host function.
    /// The trampoline drains these after the guest call and submits them to
    /// the registry owner; no parent endpoint is retained in the Store.
    pending_aliases: Vec<PreparedAliasRoute>,
    /// ADR-0114 teardown (#4228): logical inline-child aliases whose child the
    /// guest despawned, staged by the `despawn_inline_child` host function and
    /// drained beside `pending_aliases`. The trampoline retires each route
    /// through the registry owner and fans a departure notice out to its
    /// watchers — the teardown mirror of the publish path.
    pending_alias_retirements: Vec<MailboxId>,
    /// ADR-0163 §3 asset load window. `Some` for a component loaded
    /// through the trampoline (installed before `Component::instantiate`,
    /// so the guest's `init` and `wire` can pull assets); the
    /// `asset_fetch_p32` / `asset_catalog_p32` host fns serve the guest's
    /// `AssetWindow` / `AssetCatalog` surfaces from it. Closed after the
    /// guest's `wire` returns — the payload pin and ranges are dropped, so
    /// `asset_fetch` traps thereafter, while the catalog metadata is
    /// retained for the instance's life so `asset_catalog` still answers.
    /// `None` on the test paths that build a bare ctx.
    pub load_window: Option<LoadWindow>,
}

/// The mailbox-name prefix every wasm component (loaded or spawned)
/// registers under: `aether.embedded:<name>` — the embedding-host scope
/// namespace (ADR-0099 §5/§6, ADR-0119). The `spawn_sibling` host fn
/// (ADR-0097) needs this string to predict a spawned sibling's
/// `MailboxId = fold(host_carry, hash("{prefix}:{subname}"))`
/// synchronously. It **forward-feeds** the sole owner of the literal,
/// [`EMBEDDED_SCOPE`](aether_actor::EMBEDDED_SCOPE), which sits below this
/// crate, so substrate and the capabilities-layer `WasmTrampoline` now
/// reference one const instead of mirroring two literals; capabilities'
/// `trampoline_namespace_matches_substrate` test guards the match.
pub const TRAMPOLINE_NAMESPACE: &str = aether_actor::EMBEDDED_SCOPE;

/// ADR-0097: a sibling-spawn request the `spawn_sibling` host fn stages
/// onto [`ComponentCtx`] for the trampoline to drain and execute.
/// `parent` / `parent_name` are the validated executing actor identity the
/// child extends, not necessarily the physical trampoline root. `tag`
/// selects the exported type at `init_typed_p32`; `subname` is the resolved
/// trampoline subname and `config` is the encoded `Config` kind handed to the
/// new instance.
#[derive(Debug, Clone)]
pub struct PendingSpawn {
    pub parent: MailboxId,
    pub parent_name: String,
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
    pub fn new(sender: MailboxId, registry: Arc<Registry>, queue: Arc<Mailer>, outbound: Arc<HubOutbound>) -> Self {
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
            reply_correlation: Cell::new(Source::NO_CORRELATION),
            in_flight_mail_id: Cell::new(MailId::NONE),
            in_flight_root: Cell::new(MailId::NONE),
            reply_lineage_counter: Cell::new(REPLY_LINEAGE_BASE),
            pending_spawns: Vec::new(),
            pending_aliases: Vec::new(),
            pending_alias_retirements: Vec::new(),
            load_window: None,
        }
    }

    pub(crate) fn stage_alias(&mut self, alias: PreparedAliasRoute) {
        self.pending_aliases.push(alias);
    }

    pub(crate) fn take_pending_aliases(&mut self) -> Vec<PreparedAliasRoute> {
        mem::take(&mut self.pending_aliases)
    }

    pub(crate) fn has_pending_alias(&self, alias: MailboxId) -> bool {
        self.pending_aliases.iter().any(|pending| pending.alias == alias && pending.target_parent == self.sender)
    }

    /// Rendered identity for a validated actor in this component cluster.
    /// A nested child may spawn from its immediate `wire` before the registry
    /// owner publishes that child's alias, so consult locally prepared routes
    /// before the owner-visible reverse map.
    pub(crate) fn cluster_actor_name(&self, actor: MailboxId) -> Option<String> {
        if actor == self.sender {
            return self.registry.mailbox_name(actor);
        }
        self.pending_aliases
            .iter()
            .find(|pending| pending.alias == actor && pending.target_parent == self.sender)
            .map(|pending| pending.rendered_name.to_string())
            .or_else(|| {
                if self.registry.is_alias_to(actor, self.sender) {
                    self.registry.mailbox_name(actor)
                } else {
                    None
                }
            })
    }

    /// Stage the retirement of `alias`, whose inline child the guest just
    /// despawned. A spawn and a despawn inside one guest call cancel out
    /// against the still-unstaged publication rather than publishing a route
    /// only to retire it a moment later, so the owner never sees an alias that
    /// was never addressable.
    pub(crate) fn stage_alias_retirement(&mut self, alias: MailboxId) {
        let staged = self.pending_aliases.len();
        self.pending_aliases.retain(|pending| pending.alias != alias || pending.target_parent != self.sender);
        if self.pending_aliases.len() == staged {
            self.pending_alias_retirements.push(alias);
        }
    }

    pub(crate) fn take_pending_alias_retirements(&mut self) -> Vec<MailboxId> {
        mem::take(&mut self.pending_alias_retirements)
    }

    /// Install the ADR-0163 asset load window before
    /// `Component::instantiate`, so the guest's `init` and `wire` can pull
    /// asset bytes through the `asset_fetch_p32` host fn. Called by
    /// `WasmTrampoline::init`, mirroring [`Self::install_binding`].
    pub fn install_load_window(&mut self, window: LoadWindow) {
        self.load_window = Some(window);
    }

    /// Close the asset load window when the guest's `wire` returns
    /// (ADR-0163 §3): drop the payload pin and byte ranges so
    /// `asset_fetch` no longer serves, retaining the catalog metadata for
    /// the instance's life so `asset_catalog` still answers. Idempotent; a
    /// no-op when no window was installed.
    pub fn close_load_window(&mut self) {
        if let Some(window) = self.load_window.as_mut() {
            window.close();
        }
    }

    /// Wire the trampoline's `NativeBinding` into the ctx so the
    /// reply / outbound-mail host fns (in
    /// [`crate::actor::wasm::host_fns`]) can route through it. Called
    /// by `WasmTrampoline::init` (in
    /// `aether-component`) right after constructing the ctx and before
    /// `Component::instantiate` — the host-fn closure captures the ctx
    /// via the wasmtime `Store` data pointer at instantiation time,
    /// not at host-fn call time, so installing later than that is
    /// fine. Promoted from `pub(crate)` to `pub` by issue 654 when the
    /// trampoline moved to `aether-component` next to its only
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

    /// Correlation id echoed on the reply currently being dispatched,
    /// or `0` when the inbound is not a reply envelope.
    pub fn reply_correlation(&self) -> u64 {
        self.reply_correlation.get()
    }

    /// Dispatch mail. If the recipient is a sink, the handler runs inline
    /// on the caller's thread. Otherwise defer to the mailer, which
    /// routes to the component's inbox, warn-drops dropped/unknown
    /// mailboxes, or bubbles unknown ids up to the hub-substrate when
    /// a `HubOutbound` is wired (ADR-0037).
    pub fn send(&self, recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32, from: MailboxId) {
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
        self.send_routed(recipient, kind, payload, count, reply_to, mail_id, false, identity);
    }

    /// ADR-0080 §7 fire-and-forget escape hatch: the detached
    /// counterpart of [`Self::send`]. Routes the guest's send without
    /// inheriting the in-flight dispatch's lineage, so the recipient
    /// starts a fresh causal chain. Reached from the `send_mail_p32`
    /// host fn when the guest sets the detached flag (`WasmActorMailbox::
    /// send_detached`). Correlation / reply-routing are identical to
    /// `send` — only the trace lineage differs. `from` (issue 1987) is the
    /// guest-carried dispatch identity, resolved the same way as in `send`.
    pub fn send_detached(&self, recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32, from: MailboxId) {
        let correlation = self.mint_correlation();
        let identity = self.dispatch_identity(from);
        let reply_to = Source::with_correlation(SourceAddr::Component(identity), correlation);
        let mail_id = MailId::new(identity, correlation);
        self.send_routed(recipient, kind, payload, count, reply_to, mail_id, true, identity);
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
        self.send_routed(recipient, kind, payload, count, reply_to, mail_id, false, identity);
    }

    /// Shared routing body of [`Self::send`] and [`Self::reply`]: stamp
    /// the inbound lineage, offer lifecycle-authored mail to the staged
    /// activation hold, then fire the ADR-0080 §2 `Sent` hook and dispatch
    /// by recipient class (inline sink, actor inbox, or dropped/unknown
    /// bubble-up). The caller supplies the `reply_to`
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
        let mail =
            Mail::new(recipient, kind, payload, count).with_reply_to(reply_to).with_lineage(mail_id, root, parent_mail);

        // ADR-0165: guest `wire` runs before this actor's route is
        // authoritatively Live. A trampoline-backed ctx therefore offers its
        // fully-stamped mail to the binding's existing activation hold. The
        // hold check and append share one lock with release: rejection means
        // release already won, so this mail may take the ordinary eager path.
        let mail = if let Some(binding) = &self.binding {
            match binding.try_hold_component_mail(mail, identity) {
                Some(mail) => mail,
                None => return,
            }
        } else {
            mail
        };

        // Issue 1987: the recorded source + the `origin` name stamped below
        // read the dispatch `identity` the caller resolved from the guest's
        // `from`, so an inline child's mail is attributed to the child's
        // address; a normally-addressed actor's is its own id.
        self.queue.record_sent(mail_id, root, parent_mail, identity, recipient, kind);
        Self::dispatch_routed_mail(&self.registry, &self.queue, mail, identity);
    }

    /// Dispatch one component-originated mail after its `Sent` accounting has
    /// been recorded. Kept as the shared eager/release tail so a mail retained
    /// during staged activation preserves the same origin, reply, lineage, and
    /// recipient-class behavior as an ordinary Live component send.
    pub(crate) fn dispatch_routed_mail(registry: &Registry, queue: &Mailer, mail: Mail, identity: MailboxId) {
        // Closure-bound (actor-enqueue) and Sink-bound (synchronous handler)
        // recipients dispatch inline here, bypassing the mailer's full route.
        // Issue 838: `Sink` gets a `Received`/`Finished` bracket so the chain's
        // `in_flight` balances; `Closure` does NOT because the actor's
        // downstream dispatch loop records the bracket. See [`MailboxEntry`]
        // docs for the contract.
        match registry.entry(mail.recipient) {
            Some(MailboxEntry::Inbox { handler, .. }) => {
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
                let origin = registry.mailbox_name(identity);
                // ADR-0094: the second of two production mint sites
                // (ComponentCtx's inline send bypasses `route_mail`). Armed
                // here; the recipient actor's dispatcher discharges it.
                handler.enqueue(OwnedDispatch::armed(
                    mail.kind,
                    origin,
                    mail.reply_to,
                    mail.payload,
                    mail.count,
                    mail.mail_id,
                    mail.root,
                    mail.parent_mail,
                    // iamacoffeepot/aether#1134: the second production
                    // deposit chokepoint (ComponentCtx's inline send
                    // bypasses `route_mail`), so stamp the deposit instant
                    // + scheduler backlog here too — else the recipient's
                    // `Received` would read a zeroed `t_enqueue`.
                    queue.now_nanos(),
                    pending_depth(),
                    mail.recipient,
                ));
                return;
            }
            Some(MailboxEntry::Inline(handler)) => {
                let origin = registry.mailbox_name(identity);
                handler.dispatch(crate::mail::registry::MailDispatch {
                    kind: mail.kind,
                    origin: origin.as_deref(),
                    sender: mail.reply_to,
                    payload: mail.payload.bytes(),
                    count: mail.count,
                    mail_id: mail.mail_id,
                    root: mail.root,
                    parent_mail: mail.parent_mail,
                });
                // ADR-0080 §2 settlement hook. Inline mailboxes have no
                // per-actor trace ring, so post-ADR-0086 Phase 3c their
                // Received/Finished trace events aren't recorded — only
                // settlement accounting runs here.
                queue.record_finished(mail.mail_id, mail.root);
                return;
            }
            Some(MailboxEntry::Dropped) | None => {
                // Falls through to the `queue.push` path below
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
        queue.push(mail);
    }

    /// Set the in-flight `(mail_id, root)` context the next
    /// [`Self::send`] will read for `parent_mail` + `inherited_root`.
    /// Called by [`super::Component::deliver`] right before the guest's
    /// `receive_p32` shim runs. Pre-issue-722 `ComponentCtx::send`
    /// stamped [`MailId::NONE`]; setting these cells ahead of the call
    /// makes guest-triggered sends visible to the trace observer with
    /// the correct parent edge.
    pub(crate) fn set_in_flight(&self, mail_id: MailId, root: MailId) {
        self.in_flight_mail_id.set(mail_id);
        self.in_flight_root.set(root);
    }

    /// Set the current dispatch's reply correlation. Only reply envelopes
    /// expose their correlation; request mail from a component carries the
    /// requester's id space and must not be surfaced to the recipient as its
    /// own pending-key space.
    pub(crate) fn set_reply_correlation(&self, source: Source) {
        let correlation = if matches!(source.addr, SourceAddr::None) && source.correlation_id != Source::NO_CORRELATION
        {
            source.correlation_id
        } else {
            Source::NO_CORRELATION
        };
        self.reply_correlation.set(correlation);
    }

    /// Clear the in-flight context after the guest's `receive_p32`
    /// shim returns. Symmetric with [`Self::set_in_flight`].
    pub(crate) fn clear_in_flight(&self) {
        self.in_flight_mail_id.set(MailId::NONE);
        self.in_flight_root.set(MailId::NONE);
        self.reply_correlation.set(Source::NO_CORRELATION);
    }
}
