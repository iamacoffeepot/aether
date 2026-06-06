// A loaded WASM component: its wasmtime `Store<ComponentCtx>`, instance,
// and the cached handles needed to deliver mail. A small mail payload is
// written to the guest at a static `MAIL_OFFSET`; a payload too large for
// that fixed window rides an on-demand guest-heap reserve buffer the guest
// grows to fit (iamacoffeepot/aether#1337 Phase 2).
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
use crate::mail::{Mail, MailId, MailKind, MailRef, MailboxId, ReplyTarget, ReplyTo};
use crate::scheduler::pending_depth;

// Scratch-region layout in the guest's linear memory. Disjoint by
// design — the three regions are written from different lifecycle
// hooks and the offset split keeps their bounds checks obvious:
//
//   MAIL_OFFSET   = 1024   — inbound mail payload (Component::deliver)
//   STATE_OFFSET  = 8192   — ADR-0016 prior-state bytes (call_on_rehydrate)
//   CONFIG_OFFSET = 16384  — ADR-0090 init config bytes (Component::instantiate)
//
// `Component` writes a region exactly once per call, so the regions
// never need to coexist within the same wasm function activation.
// Each fixed window is small (low shadow-stack region); a mail or config
// payload too large for its window instead rides the reusable guest-heap
// reserve buffer (`kind_scratch_reserve`, iamacoffeepot/aether#1337/#1390),
// which is heap-backed and shared across the two temporally-disjoint paths.
const MAIL_OFFSET: u32 = 1024;

/// Stack headroom (bytes) reserved below the standard wasm32 1 MiB stack
/// top when sizing [`MAX_MAIL_PAYLOAD_BYTES`] — room for the `receive`
/// call chain's own frames (shallow in practice; 256 KiB is generous).
const MAIL_SCRATCH_STACK_RESERVE: usize = 256 * 1024;

/// Largest inbound payload (bytes) [`Component::deliver`] writes through the
/// guest's fixed [`MAIL_OFFSET`] scratch window (iamacoffeepot/aether#1337). A
/// payload at or below this takes the fast inline path; a larger one rides the
/// reusable guest-heap buffer (`kind_scratch_reserve`, Phase 2) instead.
///
/// `Memory::write` does not grow memory, and `MAIL_OFFSET` sits at the bottom
/// of linear memory — inside the low stack region of the standard stack-first
/// wasm32 layout (1 MiB shadow stack at `[0, 1 MiB)`, then static data, then
/// the heap). So this fast-path write is bounded by the *stack*, not the heap:
/// the payload must stay below the live stack's low-water mark. That bound
/// isn't introspectable — Rust guests don't export `__stack_pointer` (only
/// `__heap_base` / `__data_end`, which sit *above* the stack and are the wrong
/// bound for low-memory scratch) — so this is a flat threshold: the 1 MiB
/// default stack minus a stack reserve and the offset. Comparable to
/// `host_fns`'s 1 MiB `MAX_STATE_BUNDLE_BYTES`, sized down to clear the stack
/// top.
const MAX_MAIL_PAYLOAD_BYTES: usize = (1 << 20) - MAIL_SCRATCH_STACK_RESERVE - MAIL_OFFSET as usize;

/// Absolute ceiling on inbound payload bytes the substrate will deliver at all
/// (iamacoffeepot/aether#1337). Between [`MAX_MAIL_PAYLOAD_BYTES`] and this,
/// mail rides the reusable guest-heap buffer; above it, mail is dropped with a
/// loud log rather than asking the guest to allocate a buffer that could
/// exhaust its memory and trap. The wire frame cap bounds arrivals upstream —
/// this is defense in depth. 64 MiB matches the default `AETHER_MAX_FRAME_SIZE`.
const MAX_DELIVERABLE_MAIL_BYTES: usize = 64 << 20;

/// Largest init config payload (bytes) [`Component::instantiate`] writes through
/// the guest's fixed [`CONFIG_OFFSET`] scratch window (ADR-0090, iamacoffeepot/
/// aether#1390). A config at or below this takes the fast inline path; a larger
/// one rides the same reusable guest-heap buffer the mail path uses
/// (`kind_scratch_reserve`) — config and mail are temporally disjoint (config at
/// init, before any mail), so sharing the one buffer is safe.
///
/// Sized exactly like [`MAX_MAIL_PAYLOAD_BYTES`] from `MAIL_OFFSET`: `CONFIG_OFFSET`
/// also sits in the guest's low shadow-stack region, so the fast-path write is
/// bounded by the live stack's low-water mark, not the heap. Flat threshold:
/// the 1 MiB default stack minus the same stack reserve and the (higher) offset.
const MAX_CONFIG_PAYLOAD_BYTES: usize =
    (1 << 20) - MAIL_SCRATCH_STACK_RESERVE - CONFIG_OFFSET as usize;

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

/// Hook that lets a guest mint a sibling component instance from its
/// own handler (issue 1363). Installed on [`ComponentCtx`] by the wasm
/// trampoline (`aether-capabilities`); the `spawn_child_p32` host fn
/// forwards the decoded `(subname, config)` here.
///
/// The substrate can't build a trampoline itself — `WasmTrampoline`
/// lives in `aether-capabilities`, downstream of this crate — so the
/// concrete spawn machinery is injected through this trait. The
/// trampoline's impl captures the component's own module + wasmtime
/// engine/linker + the chassis `Spawner` and runs the standard
/// instanced-spawn lifecycle (ADR-0079), producing a fresh trampoline
/// at `aether.component.trampoline:<subname>` that runs the *same*
/// wasm module — so a wasm-side session manager can grow a fleet of
/// sub-actors on demand, the same dynamic listener → session pattern a
/// native capability gets via `NativeCtx::spawn_child`.
pub trait ChildSpawner: Send + Sync {
    /// Spawn a child component instance. `subname` is the caller-chosen
    /// instance segment (`None` ⇒ a `Spawner`-allocated counter, like
    /// `Subname::Counter`); `config` is the wire-encoded init-config
    /// payload, the same byte-carrier as `LoadComponent.config`.
    /// `parent` is the spawning component's own mailbox, stamped onto
    /// the child's `ReplyTo` so the child's replies route back to it.
    ///
    /// Returns the child's [`MailboxId`] on success, or a human-readable
    /// error string (subname collision / retired, init failure) the
    /// host fn logs before reporting failure to the guest.
    fn spawn_child(
        &self,
        subname: Option<&str>,
        config: Vec<u8>,
        parent: MailboxId,
    ) -> Result<MailboxId, String>;
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
    /// Set by the `save_state` host fn during `on_replace`. The
    /// substrate extracts it after hooks return via
    /// `Component::take_saved_state`. Never read by the guest —
    /// rehydration reads from a scratch offset written by the
    /// substrate, not from here.
    pub saved_state: Option<StateBundle>,
    /// Set by the `save_state` host fn when it rejects a call (1 MiB
    /// cap exceeded, OOB pointer). ADR-0016 §4: a failing save aborts
    /// the replace; the substrate checks this after `on_replace` and
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
    /// Issue 1363: child-spawn hook installed by the wasm trampoline so
    /// the `spawn_child_p32` host fn can mint a sibling component
    /// instance. `Some` for ctx instances built by `WasmTrampoline`;
    /// `None` for test paths and any guest whose host that didn't wire
    /// one — in which case `spawn_child_p32` reports failure to the
    /// guest rather than silently dropping.
    pub child_spawner: Option<Arc<dyn ChildSpawner>>,
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
}

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
            child_spawner: None,
            correlation_counter: Cell::new(1),
            in_flight_mail_id: Cell::new(MailId::NONE),
            in_flight_root: Cell::new(MailId::NONE),
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

    /// Install the child-spawn hook (issue 1363). Called by
    /// `WasmTrampoline::init` (in `aether-capabilities`) right after
    /// `install_binding`, before `Component::instantiate` — like the
    /// binding, the host-fn closure captures the ctx via the wasmtime
    /// `Store` data pointer at instantiation time, so installing it
    /// before instantiate is sufficient. Replace rebuilds the ctx and
    /// re-installs, so a hot-swapped guest keeps the ability to spawn.
    pub fn install_child_spawner(&mut self, spawner: Arc<dyn ChildSpawner>) {
        self.child_spawner = Some(spawner);
    }

    /// Issue 1363: forward a guest `spawn_child_p32` call to the
    /// installed [`ChildSpawner`]. Returns the child's [`MailboxId`] on
    /// success. `Err` carries a diagnostic string — either "no spawner
    /// wired" (a guest whose host didn't install one, e.g. a test
    /// fixture) or the spawn lifecycle's own failure. The host fn maps
    /// the result to the guest's status code.
    pub fn spawn_child(
        &self,
        subname: Option<&str>,
        config: Vec<u8>,
    ) -> Result<MailboxId, String> {
        let Some(spawner) = self.child_spawner.as_ref() else {
            return Err(String::from(
                "spawn_child: no child-spawner wired on this component ctx",
            ));
        };
        spawner.spawn_child(subname, config, self.sender)
    }

    /// Mint the next correlation id and bump the counter. Private —
    /// callers that want a correlation use `ComponentCtx::send`,
    /// which mints internally and tags the outgoing mail.
    fn mint_correlation(&self) -> u64 {
        let id = self.correlation_counter.get();
        self.correlation_counter.set(id + 1);
        id
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
    pub fn send(&self, recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32) {
        // ADR-0042: mint a fresh correlation_id for this send and
        // stash it on `last_correlation` so `prev_correlation_p32`
        // can return it to the guest. The minted id rides on the
        // outgoing `ReplyTo.correlation_id`; the reply's echo
        // (auto-routed by `Mailer::send_reply`) carries it back so a
        // handler can match the reply to this send.
        let correlation = self.mint_correlation();
        let reply_to = ReplyTo::with_correlation(ReplyTarget::Component(self.sender), correlation);

        // ADR-0080 §1 (issue iamacoffeepot/aether#722): mint the
        // outbound's MailId from the same correlation that drives
        // reply routing — symmetric with `NativeBinding::send_mail_with_lineage`,
        // which uses one counter for both. The in-flight cells were
        // populated by `Component::deliver` for guest-triggered sends
        // (and remain `NONE` for substrate-internal call sites that
        // bypass `deliver`, e.g. test fixtures).
        let mail_id = MailId::new(self.sender, correlation);
        let parent_mail = match self.in_flight_mail_id.get() {
            id if id == MailId::NONE => None,
            id => Some(id),
        };
        let inherited_root = match self.in_flight_root.get() {
            id if id == MailId::NONE => None,
            id => Some(id),
        };
        let root = inherited_root.unwrap_or(mail_id);
        self.queue
            .record_sent(mail_id, root, parent_mail, self.sender, recipient, kind);

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
                // rides on `reply_to.target` so sink handlers that want
                // to reply (ADR-0041's io sink is the motivating case)
                // can route `*Result` back to this component via
                // `Mailer::send_reply`.
                //
                // iamacoffeepot/aether#848: handler is
                // `Arc<dyn InboxHandler>`; build an [`OwnedDispatch`]
                // and move payload + kind_name into it. The bytes
                // flow straight into the downstream cap's mpsc
                // envelope without a `to_vec()` clone.
                let origin = self.registry.mailbox_name(self.sender);
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
                let origin = self.registry.mailbox_name(self.sender);
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
        //   recovered from `reply_to.target` when it's a Component
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
/// inbound because its payload exceeded the guest's `kind_scratch_ceiling`
/// (iamacoffeepot/aether#1337). The mail was dropped (logged) without
/// touching guest memory or invoking `receive`; the caller treats it as a
/// non-error so the native dispatcher still discharges settlement.
pub const DISPATCH_DROPPED_OVERSIZE: u32 = 2;

/// Offset the substrate writes prior-state bytes to before calling
/// `on_rehydrate` (ADR-0016 §3). Ordered above `MAIL_OFFSET`, but not
/// spatially isolated from it — a max-size mail write at `MAIL_OFFSET`
/// runs right over this offset. State and mail are safe to share the
/// scratch because their uses are disjoint in lifecycle phase: rehydrate
/// runs once, post-init, before any mail arrives, so the two never occupy
/// the scratch in the same wasm activation.
const STATE_OFFSET: u32 = 8192;

/// Offset the substrate writes config bytes to before calling
/// `init_with_config_p32` (ADR-0090) when the config fits the fixed inline
/// window ([`MAX_CONFIG_PAYLOAD_BYTES`]). Ordered above `STATE_OFFSET`, but the
/// mail / state / config offsets are not spatially isolated — a max-size payload
/// at `MAIL_OFFSET` writes right over both. They are safe because the three
/// regions are consumed in disjoint lifecycle phases (config@init,
/// state@rehydrate, mail@deliver) and never coexist in one wasm activation.
/// Config is written once at instantiate time, before any other host fn runs. A
/// larger config rides the guest-heap reserve buffer instead (iamacoffeepot/aether#1390).
const CONFIG_OFFSET: u32 = 16384;

/// Contract with the guest: it exports a
/// `receive(kind, ptr, byte_len, count, sender) -> u32` entrypoint
/// and a `memory` named `memory`. ADR-0013 widened the receive ABI
/// with a `sender: u32` parameter — a per-instance handle the guest
/// can pass back to `reply_mail`, or `NO_REPLY_HANDLE` for
/// component-originated mail. The `byte_len: u32` parameter (added
/// to support postcard-shaped receivers per ADR-0033's "any declared
/// kind" intent) is the total payload size the substrate wrote at
/// `ptr`, sourced from `mail.payload.len()`. Cast decoders sanity-
/// check it against `size_of::<K>() * count`; postcard decoders use
/// it as the exact slice length so a parser bug or a corrupted frame
/// can't read past the substrate-written bytes into adjacent linear
/// memory. ADR-0015 + issue 584 add optional `wire`, `unwire`,
/// `on_replace`, and `on_rehydrate` exports; the substrate calls
/// them at the right lifecycle moments when present and silently
/// skips when absent (no-op trait defaults compile down to no symbol
/// under LTO, so components that don't override stay
/// backwards-compat).
pub struct Component {
    store: Store<ComponentCtx>,
    memory: Memory,
    receive: TypedFunc<(u64, u32, u32, u32, u32), u32>,
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
    /// the trampoline (via [`Self::unwire`]) before `on_replace` on
    /// the dying instance, or before the `Component` value drops on a
    /// `DropComponent`.
    unwire: Option<TypedFunc<u64, u32>>,
    on_replace: Option<TypedFunc<(), u32>>,
    on_rehydrate: Option<TypedFunc<(u32, u32, u32), u32>>,
    /// iamacoffeepot/aether#1337 Phase 2: the guest's
    /// `kind_scratch_reserve_p32` export, used to deliver a payload larger
    /// than [`MAX_MAIL_PAYLOAD_BYTES`] into a reusable guest-heap buffer
    /// instead of the fixed [`MAIL_OFFSET`] scratch window. The same buffer
    /// carries an over-window init config (iamacoffeepot/aether#1390) —
    /// config use is temporally disjoint from mail use, so sharing it is safe.
    /// `None` for a guest too old to export it — such a guest drops oversize
    /// mail (Phase 1 disposition) and rejects an over-window config rather
    /// than risking an overrun.
    kind_scratch_reserve: Option<TypedFunc<u32, u32>>,
    /// Mailbox id stamped at instantiate-time, replayed into `wire`
    /// and `unwire` calls. Same value the guest's `init` shim received.
    self_mailbox_id: u64,
}

impl Component {
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
    /// iamacoffeepot/aether#1390: the config write routes by size, mirroring
    /// [`Component::deliver`]'s mail routing. A config at or below the fixed
    /// `CONFIG_OFFSET` window cap (`MAX_CONFIG_PAYLOAD_BYTES`) lands inline at
    /// `CONFIG_OFFSET`; a larger one (up to the `MAX_DELIVERABLE_MAIL_BYTES`
    /// ceiling) rides the same reusable guest-heap reserve buffer the mail path
    /// uses; a config past that ceiling, or to a raw-FFI guest with no reserve
    /// export, is a clean boot error (`LoadResult::Err`) — never a write or trap.
    /// Whichever pointer the config landed at is what
    /// `init_with_config_p32(mailbox_id, ptr, len)` receives.
    pub fn instantiate(
        engine: &Engine,
        linker: &Linker<ComponentCtx>,
        module: &Module,
        ctx: ComponentCtx,
        config_bytes: &[u8],
    ) -> wasmtime::Result<Self> {
        let mut store = Store::new(engine, ctx);
        let instance = linker.instantiate(&mut store, module)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("guest exports no `memory`"))?;
        let receive =
            instance.get_typed_func::<(u64, u32, u32, u32, u32), u32>(&mut store, "receive_p32")?;

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
        // iamacoffeepot/aether#1337 Phase 2: the guest's reusable-buffer export.
        // Looked up here (before the config write) because the config path below
        // routes a large config through it, mirroring `deliver`'s mail routing.
        // Present on macro-built guests (emitted by `export!`); absent on raw-FFI
        // guests, which then can't take an over-window config. The reserve
        // (`kind_scratch::reserve`) is a module-level guest allocator, ready right
        // after instantiation and independent of the actor's `init`, so calling it
        // during `instantiate` before `init` is valid; config use is temporally
        // disjoint from mail use, so sharing the one buffer is safe.
        let kind_scratch_reserve = instance
            .get_typed_func::<u32, u32>(&mut store, "kind_scratch_reserve_p32")
            .ok();
        // Wasm32 ABI carries `u32` byte lengths; config bytes are
        // bounded by guest memory size (well below `u32::MAX`).
        #[allow(clippy::cast_possible_truncation)]
        let config_len = config_bytes.len() as u32;
        let init_rc = if let Ok(init_with_config) =
            instance.get_typed_func::<(u64, u32, u32), u32>(&mut store, "init_with_config_p32")
        {
            // iamacoffeepot/aether#1390: route the config write by size, exactly
            // like `deliver` routes mail. `CONFIG_OFFSET` sits in the guest's low
            // shadow-stack region, so a config that overruns the fixed window
            // would clobber the live stack and trap on init — surfaced as a
            // generic wasm trap rather than a clear bound message.
            //   - small (<= MAX_CONFIG_PAYLOAD_BYTES): fast inline write at
            //     CONFIG_OFFSET (the current fast path).
            //   - larger (<= MAX_DELIVERABLE_MAIL_BYTES) AND a reserve export
            //     exists: write into the reusable guest-heap buffer — heap-backed,
            //     no stack clobber.
            //   - beyond the ceiling, or no reserve export (raw-FFI guest): clean
            //     boot error (surfaces as LoadResult::Err) with a structured log,
            //     never a write or trap.
            // Empty config short-circuits the write (the guest's shim still
            // receives the `(ptr, 0)` pair and handles the empty-len case).
            let config_ptr = if config_bytes.is_empty() {
                CONFIG_OFFSET
            } else if config_bytes.len() <= MAX_CONFIG_PAYLOAD_BYTES {
                memory.write(&mut store, CONFIG_OFFSET as usize, config_bytes)?;
                CONFIG_OFFSET
            } else if config_bytes.len() > MAX_DELIVERABLE_MAIL_BYTES {
                Self::log_oversize_config(
                    &store,
                    config_bytes.len(),
                    "exceeds the absolute config-size bound",
                );
                return Err(wasmtime::Error::msg(format!(
                    "guest init config of {} bytes exceeds the {MAX_DELIVERABLE_MAIL_BYTES}-byte deliverable bound",
                    config_bytes.len(),
                )));
            } else if let Some(reserve) = &kind_scratch_reserve {
                let ptr = reserve.call(&mut store, config_len)?;
                memory.write(&mut store, ptr as usize, config_bytes)?;
                ptr
            } else {
                Self::log_oversize_config(
                    &store,
                    config_bytes.len(),
                    "guest exports no kind_scratch_reserve buffer (raw-FFI guest)",
                );
                return Err(wasmtime::Error::msg(format!(
                    "guest init config of {} bytes exceeds the {MAX_CONFIG_PAYLOAD_BYTES}-byte inline window and the guest exports no reserve buffer",
                    config_bytes.len(),
                )));
            };
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
        // `Replaceable::on_replace` is the default no-op still emits
        // the symbol via `export!`, but a raw-FFI guest without the
        // macro won't. Either way: look it up, store `None` if
        // missing. (Issue 584 Phase 3 retired `on_drop` — `unwire` is
        // the pre-shutdown hook now.)
        let on_replace = instance
            .get_typed_func::<(), u32>(&mut store, "on_replace")
            .ok();
        // ADR-0016: `on_rehydrate` takes `(version, ptr, len)` — the
        // substrate writes bytes into the new instance's memory at
        // `STATE_OFFSET`, then calls the shim with `(version,
        // STATE_OFFSET, len)`.
        let on_rehydrate = instance
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
            on_replace,
            on_rehydrate,
            kind_scratch_reserve,
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
    /// (`reply_to.target = ReplyTarget::Component(_)` populated by
    /// `ComponentCtx::send` / `NativeBinding::send_mail`).
    /// Broadcast-origin and system-generated mail pass
    /// `NO_REPLY_HANDLE` so the guest's `mail.reply_to()` accessor
    /// returns `None`.
    pub fn deliver(&mut self, mail: &Mail) -> wasmtime::Result<u32> {
        // ADR-0042: carry the incoming correlation through to the
        // ReplyEntry so a subsequent `reply_mail` echoes it on the
        // outgoing reply. Session / engine mail that didn't originate
        // a correlation carries 0 — fine, echo of 0 is a no-op.
        let correlation = mail.reply_to.correlation_id;
        let entry = match &mail.reply_to.target {
            ReplyTarget::Session(token) => {
                Some(ReplyEntry::new(ReplyTarget::Session(*token), correlation))
            }
            ReplyTarget::EngineMailbox {
                engine_id,
                mailbox_id,
            } => Some(ReplyEntry::new(
                ReplyTarget::EngineMailbox {
                    engine_id: *engine_id,
                    mailbox_id: *mailbox_id,
                },
                correlation,
            )),
            ReplyTarget::Component(m) => {
                Some(ReplyEntry::new(ReplyTarget::Component(*m), correlation))
            }
            ReplyTarget::None => None,
        };
        let handle = match entry {
            Some(e) => self.store.data_mut().reply_table.allocate(e),
            None => NO_REPLY_HANDLE,
        };
        // iamacoffeepot/aether#1337: choose where in guest memory the payload
        // lands. `Memory::write` does not grow memory and `MAIL_OFFSET` sits in
        // the low stack region, so a payload that overflows the fixed scratch
        // window can't be blind-written there (it would run off the end or
        // clobber the guest's static data + heap, trapping the guest → fatal
        // substrate abort). Route by size:
        //   - small (<= MAX_MAIL_PAYLOAD_BYTES): fast inline write at MAIL_OFFSET.
        //   - larger (<= MAX_DELIVERABLE_MAIL_BYTES): a reusable guest-heap
        //     buffer via `kind_scratch_reserve` — guest-owned, no overrun.
        //   - beyond that, or no buffer export (raw-FFI guest): drop loudly.
        // A drop returns `Ok` without invoking `receive`, so the trampoline's
        // `forward_to_wasm` returns normally and the native dispatcher
        // discharges the inbound's settlement bracket — no corruption, no trap,
        // no hung caller. Kind-agnostic (not fs-specific).
        let payload_len = mail.payload.len();
        // Wasm32 ABI carries `u32` byte lengths; only read in branches where
        // `payload_len <= MAX_DELIVERABLE_MAIL_BYTES`, so the cast can't lose data.
        #[allow(clippy::cast_possible_truncation)]
        let byte_len = payload_len as u32;
        let mail_ptr = if payload_len <= MAX_MAIL_PAYLOAD_BYTES {
            MAIL_OFFSET
        } else if payload_len > MAX_DELIVERABLE_MAIL_BYTES {
            self.log_dropped_oversize(mail, payload_len, "exceeds the absolute mail-size bound");
            return Ok(DISPATCH_DROPPED_OVERSIZE);
        } else if let Some(reserve) = &self.kind_scratch_reserve {
            reserve.call(&mut self.store, byte_len)?
        } else {
            self.log_dropped_oversize(
                mail,
                payload_len,
                "guest exports no kind_scratch_reserve buffer (raw-FFI guest)",
            );
            return Ok(DISPATCH_DROPPED_OVERSIZE);
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
        let result = self.receive.call(
            &mut self.store,
            (mail.kind.0, mail_ptr, byte_len, mail.count, handle),
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
            inline_cap_bytes = MAX_MAIL_PAYLOAD_BYTES,
            deliverable_cap_bytes = MAX_DELIVERABLE_MAIL_BYTES,
            reason,
            "dropping inbound mail; cannot deliver safely (see iamacoffeepot/aether#1337)",
        );
    }

    /// Loudly log an init config rejected by [`Component::instantiate`]
    /// (iamacoffeepot/aether#1390) because it could not be delivered safely —
    /// either past the absolute ceiling, or to a raw-FFI guest with no reserve
    /// buffer. Mirrors [`Self::log_dropped_oversize`]; the caller returns an
    /// `Err` that surfaces as `LoadResult::Err` rather than writing or trapping.
    /// Associated (no `&self`) because `instantiate` has no `Component` yet.
    fn log_oversize_config(store: &Store<ComponentCtx>, config_bytes: usize, reason: &str) {
        tracing::error!(
            target: "aether_substrate::component",
            mailbox_id = store.data().sender.0,
            config_bytes,
            inline_cap_bytes = MAX_CONFIG_PAYLOAD_BYTES,
            deliverable_cap_bytes = MAX_DELIVERABLE_MAIL_BYTES,
            reason,
            "rejecting init config; cannot deliver safely (see iamacoffeepot/aether#1390)",
        );
    }

    /// Issue 584 Phase 2b (ADR-0079 amended): pre-shutdown mail-allowed
    /// hook. Invoked by the trampoline before `on_replace` on the
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

    /// Invoke the guest's `on_replace` hook if it exports one.
    /// Wasmtime traps (guest panics, unreachable) are caught and
    /// logged rather than propagated — per ADR-0015, a panicking
    /// hook must not stall teardown.
    pub fn on_replace(&mut self) {
        if let Some(f) = self.on_replace.clone()
            && let Err(e) = f.call(&mut self.store, ())
        {
            tracing::error!(target: "aether_substrate::component", error = %e, "on_replace hook trapped");
        }
    }

    /// Extract the state bundle the guest deposited via `save_state`
    /// during `on_replace`. Returns `None` if `save_state` was never
    /// called (component doesn't implement migration, or the hook is
    /// a no-op). Called by the control plane *after* `on_replace`
    /// runs on the old instance — the bundle has to outlive the
    /// store.
    pub fn take_saved_state(&mut self) -> Option<StateBundle> {
        self.store.data_mut().saved_state.take()
    }

    /// Extract a failure recorded by `save_state` (size cap, OOB).
    /// `None` on clean saves and on components that didn't attempt a
    /// save. Checked by the control plane to decide whether to abort
    /// the replace (ADR-0016 §4).
    pub fn take_save_error(&mut self) -> Option<String> {
        self.store.data_mut().save_state_error.take()
    }

    /// Write the prior-state bytes into the new instance's linear
    /// memory at `STATE_OFFSET` and invoke `on_rehydrate(version,
    /// STATE_OFFSET, len)`. Returns `Ok(())` if the instance doesn't
    /// export `on_rehydrate` (ADR-0016 §3: the bundle is silently
    /// discarded when no handler claims it).
    ///
    /// ADR-0016 §4 specifies that a trap here aborts the replace, so
    /// errors are propagated rather than contained (unlike
    /// `on_replace` / `unwire`). A memory write failure — the bundle
    /// doesn't fit in the current pages — propagates too.
    pub fn call_on_rehydrate(&mut self, bundle: &StateBundle) -> wasmtime::Result<()> {
        let Some(f) = self.on_rehydrate.clone() else {
            return Ok(());
        };
        self.memory
            .write(&mut self.store, STATE_OFFSET as usize, &bundle.bytes)?;
        // Wasm32 ABI carries `u32` byte lengths; bundle bytes are
        // bounded by guest memory size (well below `u32::MAX`).
        #[allow(clippy::cast_possible_truncation)]
        let byte_len = bundle.bytes.len() as u32;
        f.call(&mut self.store, (bundle.version, STATE_OFFSET, byte_len))?;
        Ok(())
    }

    /// Read a `u32` from guest linear memory at `offset`. Test-only
    /// accessor: the production mail path writes at `MAIL_OFFSET`
    /// and the guest interprets the bytes — nothing in non-test
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
        Component::instantiate(&engine, &linker, &module, ctx(), &[]).expect("instantiate")
    }

    /// ADR-0090 helper: instantiate with explicit config bytes so a
    /// WAT-level `init_with_config_p32` can inspect what the host wrote at
    /// `CONFIG_OFFSET`.
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
        Component::instantiate(&engine, &linker, &module, ctx(), config_bytes)
    }

    /// WAT where `on_replace` writes 0x11 to offset 200 — same pattern
    /// as `control.rs` test shape but kept local so component tests
    /// stay standalone. (Issue 584 Phase 3 retired the legacy
    /// `on_drop` companion hook; pre-shutdown coverage rides
    /// [`WAT_WIRE_UNWIRE`] now.)
    const WAT_HOOKS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                i32.const 200
                i32.const 0x11
                i32.store
                i32.const 0))
    "#;

    const WAT_NO_HOOKS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0))
    "#;

    /// iamacoffeepot/aether#1337: fixture with 18 pages (1.18 MiB) of
    /// memory so a test can deliver a sub-cap payload that exceeds a
    /// single 64 KiB page (the [`MAX_MAIL_PAYLOAD_BYTES`] cap is
    /// `785_408`, larger than one page).
    const WAT_BIG_MEMORY: &str = r#"
        (module
            (memory (export "memory") 18)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0))
    "#;

    /// iamacoffeepot/aether#1337 Phase 2: fixture that exports
    /// `kind_scratch_reserve_p32` (returning a fixed high offset, standing in
    /// for the reusable heap buffer) and a `receive_p32` that records the
    /// pointer it was handed at offset 16 — so a test can prove a large payload
    /// routes through the reserve pointer rather than `MAIL_OFFSET`. 80 pages
    /// (5.2 MiB) leaves room for a >cap payload at the high offset.
    const WAT_WITH_SCRATCH_BUFFER: &str = r#"
        (module
            (memory (export "memory") 80)
            (func (export "kind_scratch_reserve_p32") (param i32) (result i32)
                i32.const 2000000)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 16
                local.get 1
                i32.store
                i32.const 0))
    "#;

    /// ADR-0090 (issue 1256): `init_with_config_p32` shim that stamps the host-
    /// provided `(config_ptr, config_len)` triple at known offsets so
    /// tests can assert the substrate wrote bytes at `CONFIG_OFFSET`
    /// and threaded the length through. Layout written by the shim:
    ///
    ///   offset 200  : low 32 bits of `mailbox_id`
    ///   offset 204  : `config_ptr` (should equal `CONFIG_OFFSET` = 16384)
    ///   offset 208  : `config_len`
    ///   offset 212  : first byte of config (when `config_len` >= 1)
    ///   offset 213  : second byte of config (when `config_len` >= 2)
    const WAT_INIT_WITH_CONFIG: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
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
    "#;

    /// iamacoffeepot/aether#1390: a guest exporting BOTH `init_with_config_p32`
    /// and `kind_scratch_reserve_p32`. The reserve returns a fixed high offset
    /// (`2_000_000`) standing in for the reusable heap buffer; `init_with_config_p32`
    /// stamps the same `(mailbox_id, config_ptr, config_len)` triple `WAT_INIT_WITH_CONFIG`
    /// does and copies the first two config bytes — so a test can prove a large
    /// config routes through the reserve pointer and round-trips its bytes to
    /// init. 80 pages (5.2 MiB) leaves room for a >inline-window config at the
    /// high offset.
    const WAT_INIT_CONFIG_WITH_RESERVE: &str = r#"
        (module
            (memory (export "memory") 80)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "kind_scratch_reserve_p32") (param i32) (result i32)
                i32.const 2000000)
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
    "#;

    /// iamacoffeepot/aether#1390: a guest exporting `init_with_config_p32` but
    /// NO `kind_scratch_reserve_p32`. 80 pages so a large config would *fit*
    /// linear memory — the rejection must come from the missing reserve export,
    /// not a memory-bound trap. Stamps the same triple so a test could inspect
    /// it on the fast path (it is never reached for the oversize case).
    const WAT_INIT_CONFIG_NO_RESERVE: &str = r#"
        (module
            (memory (export "memory") 80)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
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
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
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
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "wire") (param i64) (result i32)
                unreachable))
    "#;

    /// WAT whose `unwire` traps. Tests that `Component::unwire`
    /// contains the trap (logs but doesn't propagate), same pattern
    /// as `on_replace`'s trap-is-contained behaviour.
    const WAT_UNWIRE_TRAPS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "unwire") (param i64) (result i32)
                unreachable))
    "#;

    /// ADR-0016 save-side: `on_replace` calls `save_state` with a
    /// version and 4 bytes at offset 300 (`0xDE 0xAD 0xBE 0xEF`).
    const WAT_SAVES_STATE: &str = r#"
        (module
            (import "aether" "save_state_p32"
                (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 300) "\de\ad\be\ef")
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                (drop (call $save_state
                    (i32.const 7)    ;; version
                    (i32.const 300)  ;; ptr
                    (i32.const 4)))  ;; len
                i32.const 0))
    "#;

    /// ADR-0016 save-side: `on_replace` attempts a save larger than
    /// the 1 MiB cap. The host fn records the error on the ctx and
    /// returns status 3 (too-large). The guest drops the return.
    const WAT_SAVES_TOO_LARGE: &str = r#"
        (module
            (import "aether" "save_state_p32"
                (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                (drop (call $save_state
                    (i32.const 1)            ;; version
                    (i32.const 0)            ;; ptr
                    (i32.const 0x00200000))) ;; 2 MiB — over the cap
                i32.const 0))
    "#;

    /// ADR-0016 load-side: `on_rehydrate(version, ptr, len)` copies
    /// `len` bytes from `ptr` to offset 400 and writes `version` at
    /// offset 396. Bulk-memory (`memory.copy`) is on by default in
    /// wasmtime; no feature flag needed.
    const WAT_REHYDRATES: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
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
    "#;

    /// ADR-0013: `receive` stores the sender handle at offset 500 so
    /// the test can observe what the substrate passed through.
    const WAT_STORES_SENDER: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 500
                local.get 4
                i32.store
                i32.const 0))
    "#;

    /// ADR-0013: `receive` echoes a reply back to the sender under a
    /// caller-provided kind id. Payload is empty — the round-trip is
    /// the observable behavior. ADR-0030 Phase 2 made kind ids hashed,
    /// so the test builds the WAT with the live `kind_id_from_parts`
    /// for "test.pong" rather than a hardcoded sequential 0.
    fn wat_replies(kind_id: u64) -> String {
        format!(
            r#"
        (module
            (import "aether" "reply_mail_p32"
                (func $reply_mail (param i32 i64 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                (drop (call $reply_mail
                    (local.get 4) ;; sender handle from receive param
                    (i64.const {kind_id}) ;; hashed kind id of "test.pong"
                    (i32.const 0) ;; ptr
                    (i32.const 0) ;; len
                    (i32.const 1))) ;; count
                i32.const 0))
        "#
        )
    }

    #[test]
    fn on_replace_invokes_export_and_writes_marker() {
        let mut component = instantiate(WAT_HOOKS);
        assert_eq!(component.read_u32(200), 0);
        component.on_replace();
        assert_eq!(component.read_u32(200), 0x11);
    }

    #[test]
    fn on_replace_on_component_without_export_is_noop() {
        let mut component = instantiate(WAT_NO_HOOKS);
        // Just needs to not panic. No marker to check.
        component.on_replace();
    }

    /// ADR-0090 (issue 1256): `Component::instantiate` writes
    /// `config_bytes` at `CONFIG_OFFSET` and calls `init_with_config_p32` with
    /// `(mailbox_id, CONFIG_OFFSET, len)`. The WAT shim stamps the
    /// triple at known offsets so this test can assert each leg
    /// without a real Kind decoder in scope.
    #[test]
    fn init_with_config_p32_threads_config_ptr_len_through() {
        let payload: &[u8] = &[0xAB, 0xCD, 0xEF, 0x12, 0x34];
        let mut component = instantiate_with_config(WAT_INIT_WITH_CONFIG, payload);
        // Mailbox id stamped: test ctx uses MailboxId(0), so low 32 bits are 0.
        assert_eq!(component.read_u32(200), 0);
        // config_ptr == CONFIG_OFFSET.
        assert_eq!(component.read_u32(204), CONFIG_OFFSET);
        // config_len matches the slice the host wrote.
        let observed_len = component.read_u32(208);
        assert_eq!(observed_len as usize, payload.len());
        // The substrate physically wrote the bytes at CONFIG_OFFSET —
        // read them back through the host-side accessor.
        let observed = component.read_bytes(CONFIG_OFFSET as usize, payload.len());
        assert_eq!(observed, payload);
        // And the guest's shim could read the same bytes through
        // `(config_ptr + i)`; the two leading bytes copied via i32.load8_u
        // land at 212 + 213.
        assert_eq!(component.read_u32(212) & 0xFF, u32::from(payload[0]));
        assert_eq!(component.read_u32(213) & 0xFF, u32::from(payload[1]));
    }

    /// Companion: empty config (the trait-default `Config = ()` path)
    /// still calls `init_with_config_p32` with `(mailbox_id, CONFIG_OFFSET, 0)`.
    /// No bytes are written to the scratch region but the shim still
    /// runs and stamps the triple.
    #[test]
    fn init_with_config_p32_empty_config_passes_zero_length() {
        let mut component = instantiate_with_config(WAT_INIT_WITH_CONFIG, &[]);
        // Triple stamped, len == 0.
        assert_eq!(component.read_u32(200), 0);
        assert_eq!(component.read_u32(204), CONFIG_OFFSET);
        assert_eq!(component.read_u32(208), 0);
        // No bytes were copied to 212 / 213 (the WAT skips the copy
        // when len == 0), so the slot stays zero.
        assert_eq!(component.read_u32(212), 0);
    }

    /// iamacoffeepot/aether#1390: a config at/under [`MAX_CONFIG_PAYLOAD_BYTES`]
    /// takes the fast inline path — it lands at the fixed `CONFIG_OFFSET`, not a
    /// reserve pointer, even when the guest also exports the reserve buffer.
    #[test]
    fn instantiate_small_config_takes_fast_inline_path() {
        let payload: &[u8] = &[0x01, 0x02, 0x03];
        let mut component = instantiate_with_config(WAT_INIT_CONFIG_WITH_RESERVE, payload);
        // config_ptr == CONFIG_OFFSET (the fast window), NOT the reserve's 2_000_000.
        assert_eq!(component.read_u32(204), CONFIG_OFFSET);
        assert_eq!(component.read_u32(208) as usize, payload.len());
        // Bytes physically landed at CONFIG_OFFSET.
        assert_eq!(
            component.read_bytes(CONFIG_OFFSET as usize, payload.len()),
            payload,
        );
    }

    /// iamacoffeepot/aether#1390: a config too large for the inline window but
    /// within the deliverable bound rides the guest's `kind_scratch_reserve`
    /// buffer — `init_with_config_p32` is handed the reserve pointer (not
    /// `CONFIG_OFFSET`) and the config bytes round-trip to it.
    #[test]
    fn instantiate_large_config_routes_through_reserve_buffer() {
        // 900_000 > MAX_CONFIG_PAYLOAD_BYTES (770_048), < MAX_DELIVERABLE_MAIL_BYTES.
        let mut payload = vec![0u8; 900_000];
        payload[0] = 0xA1;
        payload[1] = 0xB2;
        let mut component = instantiate_with_config(WAT_INIT_CONFIG_WITH_RESERVE, &payload);
        // init saw the reserve pointer, not CONFIG_OFFSET.
        assert_eq!(
            component.read_u32(204),
            2_000_000,
            "large config should route via the reserve buffer pointer",
        );
        assert_eq!(component.read_u32(208) as usize, payload.len());
        // The substrate physically wrote the config at the reserve pointer.
        assert_eq!(component.read_bytes(2_000_000, 4), payload[..4]);
        // The guest's shim read the first two bytes back through (config_ptr + i).
        assert_eq!(component.read_u32(212) & 0xFF, u32::from(payload[0]));
        assert_eq!(component.read_u32(213) & 0xFF, u32::from(payload[1]));
    }

    /// iamacoffeepot/aether#1390: a config past the absolute ceiling is a clean
    /// boot error — `instantiate` returns `Err` (→ `LoadResult::Err`) without
    /// writing or trapping. (Uses a small fixture: the guard fires on the length
    /// check before any write, regardless of guest memory size.)
    #[test]
    fn instantiate_oversize_config_returns_clean_error() {
        let payload = vec![0u8; MAX_DELIVERABLE_MAIL_BYTES + 1];
        // `Component` is not `Debug`, so match rather than `expect_err`.
        let Err(err) = try_instantiate_with_config(WAT_INIT_WITH_CONFIG, &payload) else {
            panic!("oversize config must fail to instantiate");
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("exceeds") && msg.contains("deliverable"),
            "error should name the deliverable bound; got: {msg}",
        );
    }

    /// iamacoffeepot/aether#1390: a config too large for the inline window
    /// delivered to a guest with NO `kind_scratch_reserve` export is a clean boot
    /// error, not a trap — the guard fires before the write even though the
    /// 80-page guest has room for the bytes.
    #[test]
    fn instantiate_large_config_without_reserve_returns_clean_error() {
        let payload = vec![0u8; MAX_CONFIG_PAYLOAD_BYTES + 1];
        // `Component` is not `Debug`, so match rather than `expect_err`.
        let Err(err) = try_instantiate_with_config(WAT_INIT_CONFIG_NO_RESERVE, &payload) else {
            panic!("large config without a reserve export must fail");
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("no reserve buffer"),
            "error should name the missing reserve buffer; got: {msg}",
        );
    }

    #[test]
    fn on_replace_save_state_populates_bundle() {
        let mut component = instantiate(WAT_SAVES_STATE);
        assert!(component.take_saved_state().is_none());
        component.on_replace();
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
    /// `unwire` export. Trampoline calls this before `on_replace`
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
    /// `on_replace` traps are — logged but not propagated (per
    /// ADR-0015, panicking hooks must not stall teardown).
    #[test]
    fn unwire_trap_is_contained() {
        let mut component = instantiate(WAT_UNWIRE_TRAPS);
        // `unreachable` traps; substrate logs and continues. Reaching
        // the line after the call is the whole assertion.
        component.unwire();
    }

    /// Issue 584 Phase 2b: a component without a `wire` / `unwire`
    /// export is a no-op (matches the `on_replace` pattern).
    #[test]
    fn unwire_on_component_without_export_is_noop() {
        let mut component = instantiate(WAT_NO_HOOKS);
        component.unwire();
    }

    /// iamacoffeepot/aether#1337: a payload at/under [`MAX_MAIL_PAYLOAD_BYTES`]
    /// takes the fast inline path — the guest's `receive` runs and returns 0.
    /// Guards against the threshold being so tight it rejects legitimate mail
    /// the inline window can hold (e.g. a ~739 KiB mesh).
    #[test]
    fn deliver_accepts_payload_under_scratch_ceiling() {
        let mut component = instantiate(WAT_BIG_MEMORY);
        // 700_000 < 785_408 threshold, and fits the 18-page memory.
        let mail = Mail::new(MailboxId(0), aether_data::KindId(0), vec![0u8; 700_000], 1);
        let rc = component.deliver(&mail).expect("deliver ok");
        assert_eq!(rc, 0, "guest receive should have run inline");
    }

    /// iamacoffeepot/aether#1337 Phase 2: a payload too large for the inline
    /// window but within the deliverable bound is carried through the guest's
    /// `kind_scratch_reserve` buffer — `receive` runs (rc 0) and is handed the
    /// pointer the reserve export returned, not `MAIL_OFFSET`.
    #[test]
    fn deliver_routes_large_payload_through_reserve_buffer() {
        let mut component = instantiate(WAT_WITH_SCRATCH_BUFFER);
        // 900_000 > MAX_MAIL_PAYLOAD_BYTES (785_408), < MAX_DELIVERABLE_MAIL_BYTES.
        let mail = Mail::new(MailboxId(0), aether_data::KindId(0), vec![0u8; 900_000], 1);
        let rc = component.deliver(&mail).expect("deliver ok");
        assert_eq!(rc, 0, "guest receive should have run");
        // The fixture's `receive` recorded the pointer it was handed at offset 16.
        assert_eq!(
            component.read_u32(16),
            2_000_000,
            "large payload should route via the reserve buffer pointer",
        );
    }

    /// A guest that exports no `kind_scratch_reserve` (raw-FFI, pre-Phase-2)
    /// drops an over-inline-window payload cleanly rather than trapping on the
    /// write — the guard fires before `Memory::write`, regardless of guest
    /// memory size (single-page fixture here).
    #[test]
    fn deliver_drops_large_payload_without_reserve_export() {
        let mut component = instantiate(WAT_NO_HOOKS);
        let mail = Mail::new(
            MailboxId(0),
            aether_data::KindId(0),
            vec![0u8; MAX_MAIL_PAYLOAD_BYTES + 1],
            1,
        );
        let rc = component.deliver(&mail).expect("deliver must not trap");
        assert_eq!(rc, DISPATCH_DROPPED_OVERSIZE);
    }

    #[test]
    fn on_replace_save_state_without_export_leaves_bundle_empty() {
        let mut component = instantiate(WAT_NO_HOOKS);
        component.on_replace();
        assert!(component.take_saved_state().is_none());
        assert!(component.take_save_error().is_none());
    }

    #[test]
    fn save_state_over_cap_records_error_and_no_bundle() {
        let mut component = instantiate(WAT_SAVES_TOO_LARGE);
        component.on_replace();
        let err = component.take_save_error().expect("error recorded");
        assert!(err.contains("exceeds"), "got: {err}");
        assert!(component.take_saved_state().is_none());
    }

    #[test]
    fn call_on_rehydrate_writes_bytes_and_invokes_hook() {
        let mut component = instantiate(WAT_REHYDRATES);
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

        let mut component = instantiate(WAT_STORES_SENDER);
        // Mail::new defaults sender to SessionToken::NIL.
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1);
        component.deliver(&mail).expect("deliver");
        assert_eq!(component.read_u32(500), NO_REPLY_HANDLE);
    }

    #[test]
    fn deliver_with_real_token_allocates_session_handle() {
        use crate::actor::wasm::reply_table::{NO_REPLY_HANDLE, ReplyEntry};
        use crate::mail::{Mail as SubstrateMail, MailboxId as M, ReplyTarget, ReplyTo};
        use aether_data::{SessionToken, Uuid};

        let mut component = instantiate(WAT_STORES_SENDER);
        let token = SessionToken(Uuid::from_u128(0xaaaa));
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
            .with_reply_to(ReplyTo::to(ReplyTarget::Session(token)));
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
        use crate::mail::{Mail as SubstrateMail, MailboxId as M, ReplyTarget, ReplyTo};

        let mut component = instantiate(WAT_STORES_SENDER);
        // ADR-0017 / issue #644: component-origin mail (peer-to-peer
        // send sets `reply_to.target = Component(sender)`) gets a
        // Component-variant handle.
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
            .with_reply_to(ReplyTo::to(ReplyTarget::Component(M(7))));
        component.deliver(&mail).expect("deliver");
        let observed = component.read_u32(500);
        assert_ne!(observed, NO_REPLY_HANDLE);
        assert_eq!(
            component.store.data().reply_table.resolve(observed),
            Some(ReplyEntry::component(M(7))),
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
        Component::instantiate(&engine, &linker, &module, ctx, &[]).unwrap()
    }

    #[test]
    fn reply_mail_emits_session_addressed_frame() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M, ReplyTarget, ReplyTo};
        use aether_data::{SessionToken, Uuid};

        let (ctx, rx, pong_id) = plane_ctx_for_reply();
        let mut component = instantiate_with_ctx(&wat_replies(pong_id.0), ctx);

        let token = SessionToken(Uuid::from_u128(0xbeef));
        let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
            .with_reply_to(ReplyTo::to(ReplyTarget::Session(token)));
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

    /// ADR-0037 Phase 1 + Phase 2: when a component sends to a mailbox
    /// id the local registry doesn't know, `ctx.send` defers to the
    /// mailer, which emits an upstream `MailToHubSubstrate` frame
    /// carrying the sender's mailbox id so the hub can build a
    /// `ReplyTo::EngineMailbox` for the receiving component.
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
        ctx.send(unknown, kind, vec![1, 2, 3], 1);

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

        ctx.send(sink_id, aether_data::KindId(0xABCD), vec![1, 2, 3], 1);

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

        ctx.send(sink_id, aether_data::KindId(0xCAFE), vec![], 1);

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let (mail_id, root, parent) = captured[0];
        assert!(parent.is_none(), "no inbound -> no parent edge");
        assert_eq!(root, mail_id, "fresh chain: root == mail_id");
        assert_eq!(mail_id.sender, sender);
    }

    /// Issue 1363: a stub [`ChildSpawner`] that records each
    /// `(subname, config, parent)` it's asked to spawn and hands back a
    /// fixed child id, so the host-fn tests can assert what the guest's
    /// `spawn_child_p32` call decoded out of guest memory.
    struct RecordingChildSpawner {
        calls: Arc<Mutex<Vec<(Option<String>, Vec<u8>, MailboxId)>>>,
        result: Result<MailboxId, String>,
    }

    impl ChildSpawner for RecordingChildSpawner {
        fn spawn_child(
            &self,
            subname: Option<&str>,
            config: Vec<u8>,
            parent: MailboxId,
        ) -> Result<MailboxId, String> {
            self.calls
                .lock()
                .unwrap()
                .push((subname.map(str::to_owned), config, parent));
            self.result.clone()
        }
    }

    /// iamacoffeepot/aether#1363: WAT that calls `spawn_child_p32` from
    /// its `receive` with a subname slice at offset 600 and a config
    /// slice at offset 620, then stores the returned child id (low 32
    /// bits) at offset 700 so the test can read it back.
    fn wat_spawns_child(subname: &str, config: &[u8]) -> String {
        let mut data = String::new();
        for b in subname.as_bytes() {
            data.push_str(&format!("\\{b:02x}"));
        }
        let mut config_data = String::new();
        for b in config {
            config_data.push_str(&format!("\\{b:02x}"));
        }
        let subname_len = subname.len();
        let config_len = config.len();
        format!(
            r#"
        (module
            (import "aether" "spawn_child_p32"
                (func $spawn_child (param i32 i32 i32 i32) (result i64)))
            (memory (export "memory") 1)
            (data (i32.const 600) "{data}")
            (data (i32.const 620) "{config_data}")
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 700
                (call $spawn_child
                    (i32.const 600) (i32.const {subname_len})
                    (i32.const 620) (i32.const {config_len}))
                i32.wrap_i64
                i32.store
                i32.const 0))
        "#
        )
    }

    /// Build a `ComponentCtx` wired with a recording child spawner and
    /// return the shared call log + ctx.
    fn ctx_with_child_spawner(
        result: Result<MailboxId, String>,
    ) -> (
        ComponentCtx,
        Arc<Mutex<Vec<(Option<String>, Vec<u8>, MailboxId)>>>,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = ctx();
        ctx.install_child_spawner(Arc::new(RecordingChildSpawner {
            calls: Arc::clone(&calls),
            result,
        }));
        (ctx, calls)
    }

    #[test]
    fn spawn_child_host_fn_forwards_subname_and_config() {
        let child = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x5151));
        let (ctx, calls) = ctx_with_child_spawner(Ok(child));
        let mut component =
            instantiate_with_ctx(&wat_spawns_child("worker-3", &[0xAA, 0xBB]), ctx);

        let mail = Mail::new(MailboxId(0), aether_data::KindId(0), vec![], 1);
        component.deliver(&mail).expect("deliver");

        // The guest stored the returned child id's low 32 bits at 700.
        #[allow(clippy::cast_possible_truncation)]
        let expected_low = child.0 as u32;
        assert_eq!(component.read_u32(700), expected_low);

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "spawner should have been called once");
        let (subname, config, parent) = &calls[0];
        assert_eq!(subname.as_deref(), Some("worker-3"));
        assert_eq!(config, &vec![0xAA, 0xBB]);
        // The test ctx uses MailboxId(0) as its sender.
        assert_eq!(*parent, MailboxId(0));
    }

    #[test]
    fn spawn_child_host_fn_empty_subname_is_counter() {
        let child = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x42));
        let (ctx, calls) = ctx_with_child_spawner(Ok(child));
        let mut component = instantiate_with_ctx(&wat_spawns_child("", &[]), ctx);

        let mail = Mail::new(MailboxId(0), aether_data::KindId(0), vec![], 1);
        component.deliver(&mail).expect("deliver");

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (subname, config, _parent) = &calls[0];
        // Empty subname slice ⇒ None (the Subname::Counter shape).
        assert_eq!(*subname, None);
        assert!(config.is_empty());
    }

    #[test]
    fn spawn_child_host_fn_reports_failure_as_zero() {
        let (ctx, calls) = ctx_with_child_spawner(Err("subname in use".into()));
        let mut component =
            instantiate_with_ctx(&wat_spawns_child("dup", &[]), ctx);

        let mail = Mail::new(MailboxId(0), aether_data::KindId(0), vec![], 1);
        component.deliver(&mail).expect("deliver");

        // A failed spawn returns 0 to the guest (SPAWN_CHILD_FAILED).
        assert_eq!(component.read_u32(700), 0);
        // The spawner was still consulted.
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn spawn_child_host_fn_no_spawner_wired_returns_zero() {
        // A ctx with no `install_child_spawner` call — e.g. a guest whose
        // host didn't wire one. The host fn reports failure (0) rather
        // than panicking.
        let mut component = instantiate_with_ctx(&wat_spawns_child("x", &[]), ctx());
        let mail = Mail::new(MailboxId(0), aether_data::KindId(0), vec![], 1);
        component.deliver(&mail).expect("deliver");
        assert_eq!(component.read_u32(700), 0);
    }

    /// `ComponentCtx::spawn_child` with no hook wired returns the
    /// "no spawner" error rather than reaching for a `None`.
    #[test]
    fn ctx_spawn_child_without_hook_errs() {
        let ctx = ctx();
        let err = ctx.spawn_child(Some("x"), vec![]).unwrap_err();
        assert!(err.contains("no child-spawner"), "got: {err}");
    }
}
