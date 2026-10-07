use std::cell::Cell;
use std::mem;
use std::sync::Arc;

use aether_data::Kind;
use aether_kinds::DecodeRefused;
use rustc_hash::FxHashMap;

use crate::actor::monitor::MonitorHandle;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::actor::wasm::blob_table::BlobTable;
use crate::actor::wasm::kind_manifest::Dependency;
use crate::actor::wasm::reply_table::{HeldChain, ReplyEntry, ReplyMail, ReplyOrigin, ReplyTable};
use crate::actor::wasm::watch_table::{Opened, RowOrigin, WatchPair, WatchTable};
use crate::mail::attachments::{Attachments, EncodedMail, ResolveError, plain_payload, resolve_on_send};
use crate::mail::mailer::Mailer;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{
    DispatchParts, MailboxEntry, OwnedDispatch, PreparedAliasRetirement, PreparedAliasRoute, Registry, RouteContract,
};
use crate::mail::{Mail, MailId, MailKind, MailboxId, Source, SourceAddr};
use crate::scheduler::pending_depth;

use crate::actor::wasm::asset_manifest::LoadWindow;

use super::StateBundle;
use super::meter::MemoryMeter;
use super::outbox::{GuestAnswer, HeldMail, HeldOutbox};

/// The next number in each of the two `MailId` correlation spaces a
/// mailbox's guest mints in: the next send correlation and the next
/// reply-lineage id. Read from a live component with
/// [`super::Component::correlation_cursor`] and resumed by its successor
/// with [`super::Component::resume_correlations`], so a mailbox never reuses a
/// request id or a reply's trace `MailId` within a run (ADR-0139 §3,
/// #6422). Opaque: it has no public constructor, accessor or codec, so a
/// cursor can only come from a live component and can never lower a
/// counter.
#[derive(Clone, Copy, Debug)]
pub struct CorrelationCursor {
    send: u64,
    reply_lineage: u64,
}

/// A mailbox's held reply handles and the free-slot queue its guest's next
/// handles come from. Taken from a guest leaving its slot with
/// [`super::Component::take_pending_replies`] and installed on the slot's next
/// occupant with [`super::Component::resume_replies`], so a handle stays
/// answerable to its own requester across replace and a failed start,
/// and a replacement never reissues a handle still held (#6409). Opaque: it
/// has no public constructor, accessor or codec, so it can only come from a
/// live component. Neither `Clone` nor `Copy`: two tables holding the same
/// handle would answer one request twice.
pub struct PendingReplies(ReplyTable);

/// A mailbox's watches (ADR-0079 §8): the registrations the host holds for
/// its guest and the watch ids that name them. Taken from a guest leaving its
/// slot with [`super::Component::take_watches`] and installed on the slot's
/// next occupant with [`super::Component::resume_watches`], so a watch stands
/// across a republish with no registration made or released. Opaque: it has
/// no public constructor, accessor or codec, so it can only come from a live
/// component. Neither `Clone` nor `Copy`: a registration has one owner.
pub struct Watches(WatchTable);

/// Per-component context stored as wasmtime `Store` data. Holds the
/// sender's own `MailboxId`, its binding (which reaches the shared mail
/// queue), and a handle to the registry so the `send_mail` host function
/// can route without consulting the scheduler's internals.
///
/// Deliberately does NOT hold the scheduler's full shared state — doing
/// so would create an Arc cycle through `Scheduler owns Actor, Actor
/// owns Store<ComponentCtx>, ComponentCtx back to Scheduler`. By holding
/// only its own binding and an `Arc<Registry>` the cycle is broken:
/// neither of those owns any actor.
pub struct ComponentCtx {
    pub(crate) sender: MailboxId,
    pub(crate) registry: Arc<Registry>,
    /// ADR-0013: direct outbound handle so the `reply_mail` host fn
    /// can address a specific Claude session without routing through
    /// a well-known sink. Broadcast still goes through
    /// `hub.claude.broadcast`; reply is the session-targeted twin.
    /// `HubOutbound::disconnected` when no hub is attached — sends
    /// silently drop, matching the broadcast semantics.
    pub outbound: Arc<HubOutbound>,
    /// ADR-0013 + ADR-0017: handle→entry slab populated by
    /// `Component::deliver` whenever an inbound mail has a meaningful
    /// reply target — a Claude session (`ReplyEntry::Session`) or
    /// another component (`ReplyEntry::Component`). The guest
    /// receives an opaque `u32` handle as the 4th param on its
    /// `receive` shim and passes it back to `reply_mail`; the
    /// substrate routes either over `HubOutbound` or back through
    /// `Mailer` based on the variant. An entry is freed when the guest
    /// answers it, or when `deliver` sees a single-class or unhandled
    /// dispatch return (#6412). One table per mailbox slot, not
    /// per instance: the component trampoline carries it to the slot's
    /// next occupant as [`PendingReplies`] (#6409).
    pub(crate) reply_table: ReplyTable,
    /// ADR-0238 decisions 2 and 4: the blob-store entries this instance's
    /// guest can reach, keyed by hash: those `deliver` pins for the current
    /// receive call, and those live `GuestHold`s hold, with their count. The
    /// `blob_hold_p32` / `blob_read_p32` / `blob_drop_p32` host fns resolve a
    /// guest's hash only here. One table per instance, unlike `reply_table`:
    /// a replacement starts empty, and the table drops with its instance,
    /// releasing whatever is still held.
    pub(crate) blob_table: BlobTable,
    /// ADR-0079 §8: the watches this mailbox's guest holds on other actors,
    /// which the `watch_p32` / `unwatch_p32` / `watch_ended_p32` host fns
    /// read and write. One table per mailbox slot, like `reply_table`: the
    /// component trampoline carries it to the slot's next occupant as
    /// [`Watches`], and it drops with the instance that holds it, releasing
    /// every registration with no guest code run.
    watches: WatchTable,
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
    /// correlation counter). Every ctx is built from its trampoline's
    /// binding, which also supplies [`Self::sender`] (#6521).
    pub(crate) binding: Arc<NativeBinding>,
    /// ADR-0042 correlation counter. One per mailbox slot, not per
    /// instance: a fresh slot starts at `1` so that `0` always means
    /// "no correlation" (backward-compat sentinel for replies that
    /// don't filter on correlation, and for `prev_correlation` before
    /// the slot's first send), and a replacement instance resumes its
    /// predecessor's value through [`super::Component::resume_correlations`],
    /// so a mailbox never reuses an id within a run (ADR-0139 §3). Holds the
    /// *next* id to mint; `prev_correlation()` reads `counter - 1` to
    /// return the last one minted.
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
    /// triggers stamps `parent_mail = in_flight_mail_id` and
    /// `inherited_root = in_flight_root`. Cleared back to `None` when
    /// `receive_p32` returns. Issue iamacoffeepot/aether#722.
    in_flight_mail_id: Cell<Option<MailId>>,
    /// ADR-0080 §5 in-flight inbound `root`. See `in_flight_mail_id`.
    in_flight_root: Cell<Option<MailId>>,
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
    ///
    /// One per mailbox slot, not per instance, like `correlation_counter`:
    /// a fresh slot starts at [`REPLY_LINEAGE_BASE`], and a replacement
    /// resumes its predecessor's value through
    /// [`super::Component::resume_correlations`], so its replies never reuse a trace
    /// `MailId` its predecessor already sent (#6422).
    reply_lineage_counter: Cell<u64>,
    /// ADR-0165: logical inline-child aliases staged by the host function.
    /// The trampoline drains these after the guest call and submits them to
    /// the registry owner; no parent endpoint is retained in the Store.
    pending_aliases: Vec<PreparedAliasRoute>,
    /// ADR-0114 teardown (#4228): logical inline-child aliases whose child the
    /// guest despawned, staged by the `despawn_inline_child` host function and
    /// drained beside `pending_aliases`. The trampoline retires each route
    /// through the registry owner and fans a departure notice out to its
    /// watchers — the teardown mirror of the publish path.
    pending_alias_retirements: Vec<PreparedAliasRetirement>,
    /// ADR-0231 §4: the namespace and contract each inline-child actor type
    /// of the resident module publishes, keyed by actor-type tag, so an alias
    /// staged by the `spawn_inline_child_p32` host fn carries its child
    /// type's declared namespace and rows. Installed before
    /// `Component::instantiate`; empty on the test paths that build a bare
    /// ctx, where a tag lookup always misses and no alias is staged.
    inline_children: FxHashMap<u64, InlineChildType>,
    /// ADR-0163 §3 asset load window. `Some` for a component loaded
    /// through the trampoline (installed before `Component::instantiate`,
    /// so the guest's `init` and `wire` can pull assets); the
    /// `asset_fetch_p32` / `asset_blob_p32` / `asset_catalog_p32` host fns
    /// serve the guest's `AssetWindow` / `AssetCatalog` surfaces from it.
    /// Closed after the guest's `wire` returns — the window lets go of the
    /// module's code, so `asset_fetch` and `asset_blob` trap thereafter,
    /// while the catalog metadata is retained for the instance's life so
    /// `asset_catalog` still answers. An asset blob the guest took sits in
    /// `blob_table`, not here, and outlives the close.
    /// `None` on the test paths that build a bare ctx.
    pub load_window: Option<LoadWindow>,
    /// A candidate guest's held outbox (#7067): `Some` from
    /// [`Self::hold_outbox`] until the candidate is flushed or discarded,
    /// while every send and reply it makes is held rather than sent.
    held: Option<HeldOutbox>,
    /// ADR-0243 §6: the reply handle the dispatch in progress held and the
    /// `unanswered` value the guest registered for it through the
    /// `held_unanswered_p32` host fn. [`super::Component::deliver`] takes it
    /// once `receive` returns and moves it into the slot's [`HeldChain`].
    pending_unanswered: Option<(u32, ReplyMail)>,
    /// This instance's linear memory on the engine's memory ledger, and the
    /// store's resource limiter. `Component::instantiate` installs it on the
    /// store and seeds it from the memory's size. One per instance, like
    /// `blob_table`: the row leaves the report when the instance drops.
    pub(super) memory_meter: MemoryMeter,
}

/// The declared type of one inline-child actor the resident module can
/// spawn (ADR-0231 §4): its declared `NAMESPACE`, the contract it
/// publishes on its alias, and the dependencies it declares, which the
/// `spawn_inline_child_p32` host fn checks before it stages the alias
/// (ADR-0230). Built by the trampoline from the module's exported and
/// private input groups, keyed by actor-type tag
/// (`ActorId::singleton(namespace)`), and installed on `ComponentCtx`
/// before `Component::instantiate` and again on every replace.
#[derive(Clone)]
pub struct InlineChildType {
    pub namespace: Arc<str>,
    pub contract: RouteContract,
    pub dependencies: Arc<[Dependency]>,
}

/// Issue iamacoffeepot/aether#1465: starting value of
/// [`ComponentCtx::reply_lineage_counter`]. Sits at the top half of the
/// `u64` space, above the `send` correlation counter (which starts at
/// `1` and increments once per send), so a reply's lineage `MailId`
/// never collides with one this component minted for a `send`. A run
/// would need `2^63` sends to reach this base, so the two spaces stay
/// disjoint in practice.
const REPLY_LINEAGE_BASE: u64 = 1 << 63;

/// One component-originated mail [`ComponentCtx::send_routed`] routes: the
/// recipient, kind, payload, the entries its tag-1 fields name, and count,
/// the `reply_to` and lineage `mail_id` the caller minted, whether the send
/// detaches from the in-flight chain, and the dispatch `identity` it is sent
/// as.
pub(super) struct RoutedSend {
    recipient: MailboxId,
    kind: MailKind,
    payload: Vec<u8>,
    /// ADR-0238 decision 3: the entries resolve on send found for the
    /// payload's tag-1 fields, which ride the mail to its recipient.
    attachments: Attachments,
    count: u32,
    reply_to: Source,
    mail_id: MailId,
    force_detach: bool,
    /// A held reply's own chain as `(parent_mail, root)` (ADR-0243 §6),
    /// stamped in place of the in-flight cells: the reply belongs to the
    /// requester's chain, not to whichever dispatch is running when the
    /// guest answers. `None` for every other send.
    lineage: Option<(Option<MailId>, Option<MailId>)>,
    /// The resolved dispatch identity (issue 1987) — the caller computed
    /// it from the guest-carried `from`, so the recorded source + the
    /// `origin` name read it directly.
    identity: MailboxId,
}

/// Draw the next id from a mailbox's send-correlation sequence: a request's
/// correlation, or a watch's id (ADR-0079 §8).
fn next_correlation(counter: &Cell<u64>) -> u64 {
    let id = counter.get();
    counter.set(id + 1);
    id
}

impl ComponentCtx {
    /// Build a fresh ctx with empty state-migration slots and an
    /// empty sender table. Using this over the struct literal keeps
    /// the private fields (`reply_table`, `saved_state`,
    /// `save_state_error`) internal to the wiring — callers should
    /// never set them directly. The ctx's own position, and the mailer
    /// its sends go through, are read from `binding`, so the two cannot
    /// disagree.
    ///
    /// Crate-private: a guest ctx is built through
    /// [`NativeInitCtx::guest_ctx`](crate::actor::native::NativeInitCtx::guest_ctx) /
    /// [`NativeCtx::guest_ctx`](crate::actor::native::NativeCtx::guest_ctx),
    /// which read `registry` from the binding's own mailer.
    pub(crate) fn new(binding: Arc<NativeBinding>, registry: Arc<Registry>, outbound: Arc<HubOutbound>) -> Self {
        Self {
            sender: binding.self_mailbox(),
            registry,
            outbound,
            reply_table: ReplyTable::new(),
            blob_table: BlobTable::default(),
            watches: WatchTable::default(),
            saved_state: None,
            save_state_error: None,
            init_failure: None,
            memory_meter: MemoryMeter::new(&binding),
            binding,
            correlation_counter: Cell::new(1),
            reply_correlation: Cell::new(Source::NO_CORRELATION),
            in_flight_mail_id: Cell::new(None),
            in_flight_root: Cell::new(None),
            reply_lineage_counter: Cell::new(REPLY_LINEAGE_BASE),
            pending_aliases: Vec::new(),
            pending_alias_retirements: Vec::new(),
            inline_children: FxHashMap::default(),
            load_window: None,
            held: None,
            pending_unanswered: None,
        }
    }

    /// Hold every send and reply this guest makes from here on, rather than
    /// sending it, until [`super::Component::flush_held_outbox`] sends it or
    /// [`super::Component::discard_held_outbox`] drops it (#7067). The
    /// consumer is a republish preparing a candidate while the old guest is
    /// kept: it calls this before `Component::instantiate`, so the
    /// candidate's `init` and `on_rehydrate` are held.
    pub fn hold_outbox(&mut self) {
        self.held = Some(HeldOutbox::default());
    }

    /// Whether this guest's outbox is held.
    pub(crate) fn outbox_held(&self) -> bool {
        self.held.is_some()
    }

    /// Arm a held outbox's leak guard once the guest is instantiated, so
    /// from then on it must be flushed or discarded. Called by
    /// `Component::instantiate` on success.
    pub(super) fn arm_held_outbox(&mut self) {
        if let Some(held) = self.held.as_mut() {
            held.arm();
        }
    }

    /// Hold the guest's `answer` to `handle`, whose slot the caller reserved
    /// and whose `entry` and `chain` the reservation moved out.
    ///
    /// # Panics
    ///
    /// When the outbox is not held: the reply path reserves only then.
    pub(crate) fn hold_reply(&self, handle: u32, entry: ReplyEntry, chain: Option<HeldChain>, answer: GuestAnswer) {
        self.held.as_ref().expect("a reply is held only while the outbox is").push(HeldMail::Reply {
            handle,
            entry,
            chain,
            answer,
        });
    }

    /// Send every held mail in order and stop holding. A send held without
    /// lineage is stamped with the flushing turn's `parent` and `root`, so
    /// that chain settles only after the mail does; a detached send keeps
    /// its own chain. A reply is sent on its requester's chain when its slot
    /// held one (ADR-0243 §6), else on the flushing turn's, then its slot is
    /// freed and the requester's settlement hold released. Read through
    /// [`super::Component::flush_held_outbox`].
    ///
    /// The watches the candidate made and released take effect here too
    /// (ADR-0079 §8): its rows become the mailbox's own, the releases it
    /// recorded are applied, and each group it opened registers.
    pub(super) fn flush_held(&mut self, parent: Option<MailId>, root: Option<MailId>) {
        let Some(mut held) = self.held.take() else {
            return;
        };
        self.watches.commit_candidate();
        self.register_waiting_watches();
        for mail in held.drain() {
            match mail {
                HeldMail::Send(mut send) => {
                    if send.lineage.is_none() && !send.force_detach {
                        send.lineage = Some((parent, root));
                    }
                    self.route(send);
                }
                HeldMail::Reply { handle, entry: _, chain, answer } => {
                    let lineage = chain.as_ref().map_or((parent, root), |chain| (chain.parent, chain.root));
                    self.answer(answer, Some(lineage));
                    self.reply_table.release_reserved(handle);
                    if let Some(chain) = chain {
                        chain.release();
                    }
                }
            }
        }
    }

    /// Drop every held send, which never recorded `Sent`, and put every
    /// reserved reply slot back exactly, chain included, so the old guest
    /// answers its requester. Read through
    /// [`super::Component::discard_held_outbox`].
    ///
    /// The watches the candidate made are dropped and the releases it
    /// recorded forgotten (ADR-0079 §8), so the table that moves back to the
    /// old guest is the one it left.
    pub(super) fn discard_held(&mut self) {
        let Some(mut held) = self.held.take() else {
            return;
        };
        self.watches.discard_candidate();
        for mail in held.drain() {
            if let HeldMail::Reply { handle, entry, chain, .. } = mail {
                self.reply_table.restore(handle, entry, chain);
            }
        }
    }

    /// Record the `unanswered` value the guest registered for reply handle
    /// `handle`, which the dispatch in progress holds (ADR-0243 §6). A
    /// second registration in one dispatch replaces the first; `deliver`
    /// keeps only one naming the handle it dispatched.
    pub(crate) fn register_unanswered(&mut self, handle: u32, unanswered: ReplyMail) {
        self.pending_unanswered = Some((handle, unanswered));
    }

    /// Take the registration the dispatch that just returned made, if any.
    pub(super) fn take_unanswered(&mut self) -> Option<(u32, ReplyMail)> {
        self.pending_unanswered.take()
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

    /// The component's canonical name for a diagnostic, falling back to its
    /// tagged id when the registry has no name for it.
    pub(crate) fn actor_name(&self) -> String {
        self.registry.mailbox_name(self.sender).unwrap_or_else(|| self.sender.to_string())
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
            let rendered_name = self.registry.mailbox_name(alias).unwrap_or_else(|| alias.to_string());
            self.pending_alias_retirements.push(PreparedAliasRetirement::new(alias, rendered_name));
        }
    }

    pub(crate) fn take_pending_alias_retirements(&mut self) -> Vec<PreparedAliasRetirement> {
        mem::take(&mut self.pending_alias_retirements)
    }

    /// Install the ADR-0163 asset load window before
    /// `Component::instantiate`, so the guest's `init` and `wire` can pull
    /// asset bytes through the `asset_fetch_p32` host fn. Called by
    /// `WasmTrampoline::init` right after it builds the ctx.
    pub fn install_load_window(&mut self, window: LoadWindow) {
        self.load_window = Some(window);
    }

    /// Install the type of every inline-child actor the resident module can
    /// spawn, keyed by actor-type tag (ADR-0231 §4), before
    /// `Component::instantiate`, so an alias the guest stages from its
    /// `init` onward carries its child type's declared namespace and rows.
    /// Called by the wasm trampoline's `init` and its replace, which build
    /// the map from the module's exported and private input groups.
    pub fn install_inline_children(&mut self, children: impl IntoIterator<Item = (u64, InlineChildType)>) {
        self.inline_children = children.into_iter().collect();
    }

    /// The declared type of an inline child of actor-type `tag`, or `None`
    /// for a tag the resident module does not declare.
    pub(crate) fn inline_child(&self, tag: u64) -> Option<&InlineChildType> {
        self.inline_children.get(&tag)
    }

    /// Close the asset load window when the guest's `wire` returns
    /// (ADR-0163 §3): let go of the module's code the window read
    /// payloads from, so `asset_fetch` no longer serves, retaining the catalog metadata for
    /// the instance's life so `asset_catalog` still answers. Idempotent; a
    /// no-op when no window was installed.
    pub fn close_load_window(&mut self) {
        if let Some(window) = self.load_window.as_mut() {
            window.close();
        }
    }

    /// The next send correlation and the next reply-lineage id this
    /// mailbox's guest mints, for its successor to resume. Read through
    /// [`super::Component::correlation_cursor`].
    pub(super) fn correlation_cursor(&self) -> CorrelationCursor {
        CorrelationCursor { send: self.correlation_counter.get(), reply_lineage: self.reply_lineage_counter.get() }
    }

    /// Continue this mailbox's send correlation and reply-lineage
    /// sequences from a guest that left the slot, so this instance never
    /// mints a request id or a reply `MailId` another already used
    /// (ADR-0139 §3, #6422). Only ever raises either counter. Read through
    /// [`super::Component::resume_correlations`].
    pub(super) fn resume_correlations(&mut self, cursor: CorrelationCursor) {
        self.correlation_counter.set(cursor.send.max(self.correlation_counter.get()));
        self.reply_lineage_counter.set(cursor.reply_lineage.max(self.reply_lineage_counter.get()));
    }

    /// Move this guest's reply table out for the slot's next occupant.
    /// Read through [`super::Component::take_pending_replies`].
    ///
    /// # Panics
    ///
    /// While a held answer reserves a slot: the table would move away from
    /// the outbox that must release or restore it, stranding its requester.
    pub(super) fn take_pending_replies(&mut self) -> PendingReplies {
        assert!(
            !self.reply_table.has_reserved(),
            "a reply table moved while a held outbox reserves a slot; flush or discard the outbox first",
        );
        PendingReplies(mem::take(&mut self.reply_table))
    }

    /// Install the reply table a guest that left this slot carried, over
    /// this instance's still-empty one. Read through
    /// [`super::Component::resume_replies`].
    pub(super) fn resume_replies(&mut self, replies: PendingReplies) {
        self.reply_table = replies.0;
    }

    /// Move this guest's watches out for the slot's next occupant. Read
    /// through [`super::Component::take_watches`].
    pub(super) fn take_watches(&mut self) -> Watches {
        Watches(mem::take(&mut self.watches))
    }

    /// Install the watches a guest that left this slot carried, over this
    /// instance's still-empty table. Read through
    /// [`super::Component::resume_watches`].
    pub(super) fn resume_watches(&mut self, watches: Watches) {
        self.watches = watches.0;
    }

    /// Whether `watcher` can be mailed: this guest's own mailbox, or an
    /// inline-child alias of it the registry owner has published. A staged
    /// alias has no route yet, so a notice mailed to it would be dropped.
    fn watcher_addressable(&self, watcher: MailboxId) -> bool {
        watcher == self.sender || self.registry.is_alias_to(watcher, self.sender)
    }

    /// Watch `target` for `watcher` through the watched type whose tag is
    /// `tag` (ADR-0079 §8) and return the watch's id. A standing watch of that
    /// triple answers its own id. A new one draws its id from the mailbox's
    /// send-correlation sequence, so it is never reused and never equals a
    /// request id (ADR-0139 §3), and joins its pair's group.
    ///
    /// A pair's first watch registers through [`MonitorHandle::register`],
    /// which posts the notice itself when `target` had already closed. It
    /// waits instead while the outbox is held, since a candidate changes
    /// nothing before commit (ADR-0241 §7), and while `watcher` is an alias
    /// with no published route; [`Self::register_waiting_watches`] registers
    /// it once either ends.
    ///
    /// The caller is the `watch_p32` host fn, which has checked `watcher`
    /// and `target`.
    pub(crate) fn watch(&mut self, watcher: MailboxId, target: MailboxId, tag: u64) -> u64 {
        let held = self.outbox_held();
        let origin = if held {
            RowOrigin::Candidate
        } else {
            RowOrigin::Standing
        };
        let registers = !held && self.watcher_addressable(watcher);

        let Self { watches, binding, correlation_counter, .. } = self;
        watches.watch(
            WatchPair { watcher, target },
            tag,
            origin,
            || next_correlation(correlation_counter),
            || {
                if registers {
                    Opened::Registered(MonitorHandle::register(binding, watcher, target))
                } else {
                    Opened::Waiting
                }
            },
        )
    }

    /// End the watch `watch`, answering whether one was there. A held
    /// candidate's release of a watch it carried is recorded and applied on
    /// commit. The caller is the `unwatch_p32` host fn.
    pub(crate) fn unwatch(&mut self, watch: u64) -> bool {
        if self.outbox_held() {
            return self.watches.record_release(watch);
        }
        self.watches.release(watch)
    }

    /// End the watch the departure notice being dispatched is for: the one
    /// `watcher` holds on `target` through the watched type whose tag is
    /// `tag`. The caller is the
    /// `watch_ended_p32` host fn, which has checked `watcher`.
    pub(crate) fn end_watch(&mut self, watcher: MailboxId, target: MailboxId, tag: u64) -> Option<u64> {
        self.watches.end(WatchPair { watcher, target }, tag)
    }

    /// Register each waiting group whose watcher is now addressable, so a
    /// target that closed while it waited is noticed at once. Read through
    /// [`super::Component::register_published_watches`], and called when a
    /// held outbox is flushed.
    pub(super) fn register_waiting_watches(&mut self) {
        let held = self.outbox_held();
        let Self { watches, binding, registry, sender, .. } = self;
        watches.register_waiting(
            held,
            |watcher| watcher == *sender || registry.is_alias_to(watcher, *sender),
            |pair| MonitorHandle::register(binding, pair.watcher, pair.target),
        );
    }

    /// Mint the next correlation id and bump the counter. Private —
    /// callers that want a correlation use `ComponentCtx::send`,
    /// which mints internally and tags the outgoing mail.
    fn mint_correlation(&self) -> u64 {
        next_correlation(&self.correlation_counter)
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

    /// Return the correlation id used by the most recent
    /// `ComponentCtx::send` call. The `prev_correlation_p32` host fn
    /// surfaces this to the guest so a handler can match an inbound
    /// reply to the request it sent. Returns `0` (the "no
    /// correlation" sentinel) before the mailbox's first send; a
    /// resumed instance reports its predecessor's last id until its own
    /// first send, since the counter belongs to the mailbox.
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
    ///
    /// `payload` carries the entries its tag-1 `Blob` fields name, resolved
    /// by the host fn against this instance's blob table (ADR-0238 decision
    /// 3); they ride the mail.
    pub(crate) fn send(&self, recipient: MailboxId, kind: MailKind, payload: EncodedMail, count: u32, from: MailboxId) {
        // ADR-0042: mint a fresh correlation_id for this send and
        // stash it on `last_correlation` so `prev_correlation_p32`
        // can return it to the guest. The minted id rides on the
        // outgoing `Source.correlation_id`; the reply's echo
        // (auto-routed by `Mailer::send_reply`) carries it back so a
        // handler can match the reply to this send.
        let correlation = self.mint_correlation();
        // Issue 1987: stamp origin from the dispatch identity the guest
        // carried on the send (`from`, already resolved in-cluster by the
        // host fn — a zero / foreign claim resolved to `self.sender` there)
        // so an inline child's sends carry the child's address.
        let reply_to = Source::with_correlation(SourceAddr::Component(from), correlation);

        // ADR-0080 §1 (issue iamacoffeepot/aether#722): mint the
        // outbound's MailId from the same correlation that drives
        // reply routing — symmetric with `NativeBinding::push_envelope_buffered`,
        // which uses one counter for both.
        let mail_id = MailId::new(from, correlation);
        let EncodedMail { bytes, attachments } = payload;
        self.send_routed(RoutedSend {
            recipient,
            kind,
            payload: bytes,
            attachments,
            count,
            reply_to,
            mail_id,
            force_detach: false,
            lineage: None,
            identity: from,
        });
    }

    /// ADR-0080 §7 fire-and-forget escape hatch: the detached
    /// counterpart of [`Self::send`]. Routes the guest's send without
    /// inheriting the in-flight dispatch's lineage, so the recipient
    /// starts a fresh causal chain. Reached from the `send_mail_p32`
    /// host fn when the guest sets the detached flag (the guest's flat
    /// `send_detached` verb or `MailSender::send_detached_to`). Correlation / reply-routing are identical to
    /// `send` — only the trace lineage differs. `from` (issue 1987) is the
    /// dispatch identity the host fn already resolved, used as in `send`.
    pub(crate) fn send_detached(
        &self,
        recipient: MailboxId,
        kind: MailKind,
        payload: EncodedMail,
        count: u32,
        from: MailboxId,
    ) {
        let correlation = self.mint_correlation();
        let reply_to = Source::with_correlation(SourceAddr::Component(from), correlation);
        let mail_id = MailId::new(from, correlation);
        let EncodedMail { bytes, attachments } = payload;
        self.send_routed(RoutedSend {
            recipient,
            kind,
            payload: bytes,
            attachments,
            count,
            reply_to,
            mail_id,
            force_detach: true,
            lineage: None,
            identity: from,
        });
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
    ///
    /// A held slot's chain in `origin` (ADR-0243 §6) stamps the reply with
    /// the requester's inbound as its parent and root, whatever dispatch is
    /// in flight. The caller keeps the chain, and so its settlement hold,
    /// alive until this returns, by which time the reply's `Sent` is
    /// recorded.
    fn reply(&self, recipient: MailboxId, kind: MailKind, payload: EncodedMail, count: u32, origin: ReplyOrigin) {
        let ReplyOrigin { correlation, from, lineage } = origin;
        let reply_to = Source::with_correlation(SourceAddr::None, correlation);
        // Issue 1987: a child's reply stamps the child's identity (the
        // guest-carried `from`, already resolved in-cluster by the host fn)
        // on its lineage `MailId`, like its sends.
        let mail_id = MailId::new(from, self.next_reply_lineage());
        let EncodedMail { bytes, attachments } = payload;
        self.send_routed(RoutedSend {
            recipient,
            kind,
            payload: bytes,
            attachments,
            count,
            reply_to,
            mail_id,
            force_detach: false,
            lineage,
            identity: from,
        });
    }

    /// Answer a guest arm's refusal of its sender (ADR-0231 §11) to the
    /// refused mail's reply target, as a native arm answers it: an
    /// `aether.mail.decode_refused` naming the kind, only to a target that
    /// opted in (see `NativeBinding::refusal_listener`), stamped on the
    /// refused mail's chain so it is handled before that chain's `Settled`.
    /// It goes out as a reply of the routed recipient, through the same body
    /// as the guest's own replies, so a held outbox holds it too.
    ///
    /// The host knows the sender and the kind. Which handler the sender lacks
    /// is in the guest's own log, since the protocol the handler requires is
    /// the guest's type, so the notice's text points there.
    ///
    /// Consumer: [`super::Component::deliver`], for `DISPATCH_REFUSED_SENDER`.
    pub(super) fn answer_refused_sender(&self, env: &Envelope) {
        let Some(target) = self.binding.refusal_listener(env.sender) else {
            return;
        };

        let Some(sender) = self.binding.stamped_sender(target) else {
            return;
        };

        let sender = self.binding.actor_path(sender);
        let error = format!(
            "the sender `{sender}` does not handle every kind the handler requires of its sender; the receiver's \
             log names the first one it lacks"
        );
        let payload =
            EncodedMail { bytes: DecodeRefused { kind: env.kind, error }.encode_into_bytes(), attachments: None };
        let origin = ReplyOrigin {
            correlation: env.sender.correlation_id,
            from: env.recipient,
            lineage: Some((env.mail_id, env.root)),
        };
        self.reply(target, DecodeRefused::ID, payload, 1, origin);
    }

    /// Send the guest's `answer` to a reply handle. `lineage`, as
    /// `(parent_mail, root)`, is the chain a local answer is stamped on: a
    /// held slot's (ADR-0243 §6) or a flushing turn's; `None` inherits the
    /// dispatch in flight. A session or remote answer leaves the process
    /// and carries no lineage.
    ///
    /// Consumers: the `reply_mail_p32` host fn, and [`Self::flush_held`].
    pub(crate) fn answer(&self, answer: GuestAnswer, lineage: Option<(Option<MailId>, Option<MailId>)>) {
        match answer {
            GuestAnswer::Session { token, kind_name, payload, correlation } => {
                let origin = self.registry.mailbox_name(self.sender);
                // The guest replies in its own name: stamp its own position,
                // which the host bound it to (ADR-0230 §3), when that
                // position holds a route.
                let stamp = self.registry.stamped_sender(self.sender);
                self.outbound.egress_to_session(token, &kind_name, payload, origin, correlation, stamp);
            }
            GuestAnswer::Component { recipient, kind, payload, count, correlation, from } => {
                self.reply(recipient, kind, payload, count, ReplyOrigin { correlation, from, lineage });
            }
            GuestAnswer::Engine { engine_id, mailbox_id, kind, payload, count, correlation } => {
                // ADR-0037 Phase 2: the hub forwards the frame to the target
                // engine's connection as `HubToEngine::MailById`.
                self.outbound.egress_to_engine_mailbox(engine_id, mailbox_id, kind, payload, count, correlation);
            }
        }
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
    /// default `send` / a reply) inherits the dispatch's chain. A held
    /// reply's `lineage` (ADR-0243 §6) replaces the in-flight cells.
    ///
    /// While the outbox is held (#7067) the send is held as it is, its
    /// lineage unstamped, and [`Self::flush_held`] routes it later.
    fn send_routed(&self, send: RoutedSend) {
        if let Some(held) = &self.held {
            held.push(HeldMail::Send(send));
            return;
        }
        self.route(send);
    }

    /// Route one send now: the body of [`Self::send_routed`] past the held
    /// outbox.
    fn route(&self, send: RoutedSend) {
        let RoutedSend {
            recipient,
            kind,
            payload,
            attachments,
            count,
            reply_to,
            mail_id,
            force_detach,
            lineage,
            identity,
        } = send;
        // ADR-0080 §1 (issue iamacoffeepot/aether#722): the in-flight
        // cells were populated by `Component::deliver` for guest-triggered
        // sends (and remain `None` for substrate-internal call sites that
        // bypass `deliver`, e.g. test fixtures). ADR-0080 §7: a detached
        // send ignores them and opens its own chain. ADR-0243 §6: a held
        // reply carries the requester's chain and ignores them too.
        let (parent_mail, inherited_root) = match lineage {
            Some(lineage) => lineage,
            None if force_detach => (None, None),
            None => (self.in_flight_mail_id.get(), self.in_flight_root.get()),
        };
        let root = inherited_root.unwrap_or(mail_id);
        let mail = Mail::new(recipient, kind, payload, count)
            .with_reply_to(reply_to)
            .with_lineage(Some(mail_id), Some(root), parent_mail)
            .with_attachments(attachments);

        // ADR-0165: guest `wire` runs before this actor's route is
        // authoritatively Live. The ctx therefore offers its fully-stamped
        // mail to the binding's existing activation hold. The hold check and
        // append share one lock with release: rejection means release
        // already won, so this mail may take the ordinary eager path.
        let Some(mail) = self.binding.try_hold_component_mail(mail, identity) else {
            return;
        };

        // Issue 1987: the recorded source + the `origin` name stamped below
        // read the dispatch `identity` the caller resolved from the guest's
        // `from`, so an inline child's mail is attributed to the child's
        // address; a normally-addressed actor's is its own id.
        self.binding.publish_component_mail(mail, mail_id, root, identity);
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
        match registry.entry_at(mail.recipient) {
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
                handler.enqueue(
                    OwnedDispatch::armed(
                        DispatchParts {
                            kind: mail.kind,
                            origin,
                            sender: mail.reply_to,
                            payload: mail.payload,
                            count: mail.count,
                            mail_id: mail.mail_id,
                            root: mail.root,
                            parent_mail: mail.parent_mail,
                            // iamacoffeepot/aether#1134: the second production
                            // deposit chokepoint (ComponentCtx's inline send
                            // bypasses `route_mail`), so stamp the deposit instant
                            // + scheduler backlog here too — else the recipient's
                            // `Received` would read a zeroed `t_enqueue`.
                            t_enqueue: queue.now_nanos(),
                            enqueue_depth: pending_depth(),
                        },
                        mail.recipient,
                    )
                    .with_attachments(mail.attachments),
                );
                return;
            }
            Some(MailboxEntry::Inline(handler)) => {
                let origin = registry.mailbox_name(identity);
                // ADR-0238: an inline handler reads plain bytes, so an
                // attached payload is lent rewritten, with no frame limit.
                let attachments = mail.attachments.as_deref().unwrap_or_default();
                match plain_payload(registry, mail.kind, mail.payload.bytes(), attachments, usize::MAX) {
                    Ok(payload) => handler.dispatch(crate::mail::registry::MailDispatch {
                        kind: mail.kind,
                        origin: origin.as_deref(),
                        sender: mail.reply_to,
                        payload: &payload,
                        count: mail.count,
                        mail_id: mail.mail_id,
                        root: mail.root,
                        parent_mail: mail.parent_mail,
                    }),
                    Err(error) => tracing::error!(
                        target: "aether_substrate::mail",
                        kind = %registry.kind_label(mail.kind),
                        %error,
                        "attached mail to an inline mailbox refused",
                    ),
                }
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

    /// Resolve on send for a guest payload (ADR-0238 decision 3): the
    /// entries the tag-1 fields of `payload`, a `kind` mail, name, found in
    /// this instance's blob table among its pins and holds. `Ok(None)`, with
    /// no schema lookup and no walk, when the table is empty: a guest that
    /// pins and holds nothing cannot carry a valid tag-1 field.
    ///
    /// # Errors
    ///
    /// [`ResolveError::Unresolved`] for a hash the table neither pins nor
    /// holds, and [`ResolveError::Malformed`] for a payload that does not
    /// follow the kind's schema.
    pub(crate) fn resolve_send(&self, kind: MailKind, payload: &[u8]) -> Result<Attachments, ResolveError> {
        if self.blob_table.is_empty() {
            return Ok(None);
        }
        resolve_on_send(&self.registry, kind, payload, |hash| self.blob_table.entry(hash).cloned())
    }

    /// Set the in-flight `(mail_id, root)` context the next
    /// [`Self::send`] will read for `parent_mail` + `inherited_root`.
    /// Called by [`super::Component::deliver`] right before the guest's
    /// `receive_p32` shim runs. Pre-issue-722 `ComponentCtx::send`
    /// stamped no parent; setting these cells ahead of the call
    /// makes guest-triggered sends visible to the trace observer with
    /// the correct parent edge.
    pub(crate) fn set_in_flight(&self, mail_id: Option<MailId>, root: Option<MailId>) {
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
        self.in_flight_mail_id.set(None);
        self.in_flight_root.set(None);
        self.reply_correlation.set(Source::NO_CORRELATION);
    }
}
