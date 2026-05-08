// A loaded WASM component: its wasmtime `Store<ComponentCtx>`, instance,
// and the cached handles needed to deliver mail. Mail payloads are
// written to the guest at a static `MAIL_OFFSET`; a guest-side
// allocator is parked until an actual use case forces the question.
//
// Holds the `ComponentCtx` (per-component context stored as wasmtime
// `Store` data) and `StateBundle` (ADR-0016 state-migration payload)
// alongside the `Component` itself — the ctx is the runtime half of
// the same primitive, so it lives here rather than in a separate
// module.

use std::cell::Cell;
use std::sync::Arc;

use wasmtime::{Engine, Linker, Memory, Module, Store, TypedFunc};

use crate::actor::native::transport::NativeTransport;
use crate::actor::wasm::reply_table::{NO_REPLY_HANDLE, ReplyEntry, ReplyTable};
use crate::input::InputSubscribers;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{MailboxEntry, Registry};
use crate::mail::{Mail, MailKind, MailboxId, ReplyTarget, ReplyTo};

const MAIL_OFFSET: u32 = 1024;

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
    /// ADR-0021 subscriber sets, shared with the platform-event
    /// publisher in `main.rs`. `#[actor]`-decorated components
    /// auto-subscribe every `K::IS_INPUT` handler kind by mailing
    /// `aether.input.subscribe` from the init prologue the macro
    /// prepends (ADR-0033 phase 3); `InputCapability` processes the
    /// mail and mutates this set.
    pub input_subscribers: InputSubscribers,
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
    /// Trampoline transport whose `wait_reply` the
    /// [`crate::actor::wasm::host_fns::wait_reply_p32`] host fn delegates to.
    /// `Some` for ctx instances built by [`WasmTrampoline::init`]
    /// (issue 634 Phase 4 PR 3 — `transport.wait_reply` is now the
    /// single source of inbox / overflow / correlation-filter
    /// truth); `None` for the test paths that build `ComponentCtx`
    /// without a real trampoline (the host fn returns
    /// [`crate::actor::wasm::host_fns::WAIT_CANCELLED`] in that case, matching
    /// the pre-Phase-4 "no inbox installed" disposition).
    pub transport: Option<Arc<NativeTransport>>,
    /// ADR-0042 correlation counter. Per-component (one
    /// `ComponentCtx` per component instance). Holds the *next* id
    /// to mint; `prev_correlation()` reads `counter - 1` to return
    /// the last one minted. Starts at `1` so that `0` always means
    /// "no correlation" (backward-compat sentinel for waits that
    /// don't filter, and for `prev_correlation` before any send).
    ///
    /// `Cell` instead of `AtomicU64`: the component is single-
    /// threaded (ADR-0038 actor-per-component), so the counter is
    /// never touched from multiple threads.
    correlation_counter: Cell<u64>,
}

impl ComponentCtx {
    /// Build a fresh ctx with empty state-migration slots and an
    /// empty sender table. Using this over the struct literal keeps
    /// the private fields (reply_table, saved_state,
    /// save_state_error) internal to the wiring — callers should
    /// never set them directly.
    pub fn new(
        sender: MailboxId,
        registry: Arc<Registry>,
        queue: Arc<Mailer>,
        outbound: Arc<HubOutbound>,
        input_subscribers: InputSubscribers,
    ) -> Self {
        ComponentCtx {
            sender,
            registry,
            queue,
            outbound,
            input_subscribers,
            reply_table: ReplyTable::new(),
            saved_state: None,
            save_state_error: None,
            init_failure: None,
            transport: None,
            correlation_counter: Cell::new(1),
        }
    }

    /// Wire the trampoline's `NativeTransport` into the ctx so the
    /// [`crate::actor::wasm::host_fns::wait_reply_p32`] host fn can route through
    /// it. Called by [`WasmTrampoline::init`] right after constructing
    /// the ctx (and before `Component::instantiate`, since the host
    /// fn closure captures the ctx via the wasmtime `Store` data
    /// pointer at instantiation time, not at host-fn call time, so
    /// installing later than that is fine).
    pub fn install_transport(&mut self, transport: Arc<NativeTransport>) {
        self.transport = Some(transport);
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
    /// surfaces this to the guest so sync wrappers know what to
    /// filter on in `wait_reply_p32`. Returns `0` (the "no
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
        // (auto-routed by `Mailer::send_reply`) carries it back, and
        // `wait_reply_p32` filters on it.
        let correlation = self.mint_correlation();
        let reply_to = ReplyTo::with_correlation(ReplyTarget::Component(self.sender), correlation);

        if let Some(MailboxEntry::Closure(handler)) = self.registry.entry(recipient) {
            let kind_name = self.registry.kind_name(kind).unwrap_or_default();
            // Component-originated mail: the sender is this ctx's
            // mailbox, so its registry name is the `origin` any
            // sink cares about (ADR-0011), and the same mailbox id
            // rides on `reply_to.target` so sink handlers that want
            // to reply (ADR-0041's io sink is the motivating case)
            // can route `*Result` back to this component via
            // `Mailer::send_reply`.
            let origin = self.registry.mailbox_name(self.sender);
            handler(
                kind,
                &kind_name,
                origin.as_deref(),
                reply_to,
                &payload,
                count,
            );
            return;
        }

        // Component / dropped / unknown all funnel through `Mailer::push`:
        // - Component (ADR-0017): mail enters the recipient's inbox with
        //   `reply_to.target = Component(self.sender)` so
        //   `Component::deliver` can allocate a Component-variant
        //   `ReplyEntry`.
        // - Dropped: warn-drops in `route_mail`.
        // - Unknown (ADR-0037): bubbles up to the hub-substrate via
        //   `MailToHubSubstrate`; the `source_mailbox_id` it carries is
        //   recovered from `reply_to.target` when it's a Component
        //   variant (warn-drops otherwise).
        self.queue
            .push(Mail::new(recipient, kind, payload, count).with_reply_to(reply_to));
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

/// Offset the substrate writes prior-state bytes to before calling
/// `on_rehydrate` (ADR-0016 §3). Deliberately separated from
/// `MAIL_OFFSET` so the two scratch regions don't overlap in the
/// worst-case size. The lifetimes are also disjoint in practice —
/// rehydrate runs once, post-init, before any mail arrives — but the
/// offset split keeps out-of-bounds checks obvious.
const STATE_OFFSET: u32 = 8192;

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
/// memory. ADR-0015 adds optional `on_replace`, `on_drop`, and
/// `on_rehydrate` exports; the substrate calls them at the right
/// lifecycle moments when present and silently skips when absent
/// (no-op trait defaults compile down to no symbol under LTO, so
/// components that don't override stay backwards-compat).
pub struct Component {
    store: Store<ComponentCtx>,
    memory: Memory,
    receive: TypedFunc<(u64, u32, u32, u32, u32), u32>,
    on_replace: Option<TypedFunc<(), u32>>,
    on_drop: Option<TypedFunc<(), u32>>,
    on_rehydrate: Option<TypedFunc<(u32, u32, u32), u32>>,
}

impl Component {
    /// Instantiate a component from a compiled `Module`. `ctx` becomes
    /// the store data and is what every host function call against this
    /// component will see.
    pub fn instantiate(
        engine: &Engine,
        linker: &Linker<ComponentCtx>,
        module: &Module,
        ctx: ComponentCtx,
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
        // Issue 525 Phase 4b / issue 531: a non-zero return value
        // means the guest's `WasmActor::init` returned `Err(BootError)`
        // and staged the message via `init_failed_p32`. Drain the
        // staged string off the ctx and surface it as a wasmtime
        // error so the existing `dispatch_load_component` failure
        // path reports it via `LoadResult::Err { error }` — same
        // shape as a wasm trap, just with a more informative message.
        let mailbox_id = store.data().sender.0;
        let init_rc = if let Ok(init) = instance.get_typed_func::<u64, u32>(&mut store, "init") {
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
        // `Component::on_replace` / `on_drop` are the default no-ops
        // still emits the symbol via `export!`, but a raw-FFI guest
        // without the macro won't. Either way: look it up, store
        // `None` if missing.
        let on_replace = instance
            .get_typed_func::<(), u32>(&mut store, "on_replace")
            .ok();
        let on_drop = instance
            .get_typed_func::<(), u32>(&mut store, "on_drop")
            .ok();
        // ADR-0016: `on_rehydrate` takes `(version, ptr, len)` — the
        // substrate writes bytes into the new instance's memory at
        // `STATE_OFFSET`, then calls the shim with `(version,
        // STATE_OFFSET, len)`.
        let on_rehydrate = instance
            .get_typed_func::<(u32, u32, u32), u32>(&mut store, "on_rehydrate_p32")
            .ok();

        Ok(Self {
            store,
            memory,
            receive,
            on_replace,
            on_drop,
            on_rehydrate,
        })
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
    /// `ComponentCtx::send` / `NativeTransport::send_mail`).
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
        self.memory
            .write(&mut self.store, MAIL_OFFSET as usize, &mail.payload)?;
        let byte_len = mail.payload.len() as u32;
        self.receive.call(
            &mut self.store,
            (mail.kind.0, MAIL_OFFSET, byte_len, mail.count, handle),
        )
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

    /// Invoke the guest's `on_drop` hook if it exports one. Same trap
    /// containment as `on_replace`.
    pub fn on_drop(&mut self) {
        if let Some(f) = self.on_drop.clone()
            && let Err(e) = f.call(&mut self.store, ())
        {
            tracing::error!(target: "aether_substrate::component", error = %e, "on_drop hook trapped");
        }
    }

    /// Extract the state bundle the guest deposited via `save_state`
    /// during `on_replace`. Returns `None` if `save_state` was never
    /// called (component doesn't implement migration, or the hook is
    /// a no-op). Called by the control plane *after* `on_replace` /
    /// `on_drop` run on the old instance — the bundle has to outlive
    /// the store.
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
    /// `on_replace` / `on_drop`). A memory write failure — the bundle
    /// doesn't fit in the current pages — propagates too.
    pub fn call_on_rehydrate(&mut self, bundle: &StateBundle) -> wasmtime::Result<()> {
        let Some(f) = self.on_rehydrate.clone() else {
            return Ok(());
        };
        self.memory
            .write(&mut self.store, STATE_OFFSET as usize, &bundle.bytes)?;
        f.call(
            &mut self.store,
            (bundle.version, STATE_OFFSET, bundle.bytes.len() as u32),
        )?;
        Ok(())
    }

    /// Read a `u32` from guest linear memory at `offset`. Test-only
    /// accessor: the production mail path writes at `MAIL_OFFSET`
    /// and the guest interprets the bytes — nothing in non-test
    /// code reads guest memory directly.
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
mod tests {
    use std::sync::Arc;

    use wasmtime::{Engine, Linker};

    use super::*;
    use crate::mail::MailboxId;
    use crate::mail::mailer::Mailer;
    use crate::mail::outbound::HubOutbound;
    use crate::mail::registry::Registry;

    fn ctx() -> ComponentCtx {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(crate::handle_store::HandleStore::new(1024 * 1024));
        ComponentCtx::new(
            MailboxId(0),
            Arc::clone(&registry),
            Arc::new(Mailer::new(registry, store)),
            HubOutbound::disconnected(),
            crate::input::new_subscribers(),
        )
    }

    fn instantiate(wat: &str) -> Component {
        let engine = Engine::default();
        let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
        crate::actor::wasm::host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(wat).expect("compile WAT");
        let module = Module::new(&engine, &wasm).expect("compile module");
        Component::instantiate(&engine, &linker, &module, ctx()).expect("instantiate")
    }

    /// WAT where `on_drop` writes 0x22 to offset 204 and `on_replace`
    /// writes 0x11 to offset 200 — same pattern as `control.rs` test
    /// shape but kept local so component tests stay standalone.
    const WAT_HOOKS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                i32.const 200
                i32.const 0x11
                i32.store
                i32.const 0)
            (func (export "on_drop") (result i32)
                i32.const 204
                i32.const 0x22
                i32.store
                i32.const 0))
    "#;

    const WAT_NO_HOOKS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0))
    "#;

    const WAT_TRAP_ON_DROP: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_drop") (result i32)
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
    fn on_drop_invokes_export_and_writes_marker() {
        let mut component = instantiate(WAT_HOOKS);
        // Pre-condition: memory is zero-initialised.
        assert_eq!(component.read_u32(204), 0);
        component.on_drop();
        assert_eq!(component.read_u32(204), 0x22);
    }

    #[test]
    fn on_replace_invokes_export_and_writes_marker() {
        let mut component = instantiate(WAT_HOOKS);
        assert_eq!(component.read_u32(200), 0);
        component.on_replace();
        assert_eq!(component.read_u32(200), 0x11);
    }

    #[test]
    fn on_drop_on_component_without_export_is_noop() {
        let mut component = instantiate(WAT_NO_HOOKS);
        // Just needs to not panic. No marker to check.
        component.on_drop();
        component.on_replace();
    }

    #[test]
    fn on_drop_trap_is_contained() {
        let mut component = instantiate(WAT_TRAP_ON_DROP);
        // `unreachable` in WASM traps; substrate must log and
        // continue rather than propagate. Reaching the line after
        // the call is the whole assertion.
        component.on_drop();
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

    fn plane_ctx_for_reply() -> (
        ComponentCtx,
        std::sync::mpsc::Receiver<crate::mail::outbound::EgressEvent>,
        aether_data::KindId,
    ) {
        use crate::mail::MailboxId as M;
        use aether_data::{KindDescriptor, SchemaType};

        let (outbound, rx) = crate::mail::outbound::HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let pong_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "test.pong".into(),
                schema: SchemaType::Unit,
                is_stream: false,
            })
            .expect("register kind");
        let store = Arc::new(crate::handle_store::HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let ctx = ComponentCtx::new(
            M(0),
            registry,
            mailer,
            outbound,
            crate::input::new_subscribers(),
        );
        (ctx, rx, pong_id)
    }

    fn instantiate_with_ctx(wat: &str, ctx: ComponentCtx) -> Component {
        let engine = Engine::default();
        let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
        crate::actor::wasm::host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(wat).unwrap();
        let module = Module::new(&engine, &wasm).unwrap();
        Component::instantiate(&engine, &linker, &module, ctx).unwrap()
    }

    #[test]
    fn reply_mail_emits_session_addressed_frame() {
        use crate::mail::outbound::EgressEvent;
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
        use crate::mail::outbound::EgressEvent;

        let (outbound, outbound_rx) = HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let sender = registry
            .try_register_closure("client", Arc::new(|_, _, _, _, _, _| {}))
            .expect("register client mailbox");

        let store = Arc::new(crate::handle_store::HandleStore::new(1024 * 1024));
        let mailer = Arc::new(
            Mailer::new(Arc::clone(&registry), store).with_outbound(Arc::clone(&outbound)),
        );

        let ctx = ComponentCtx::new(
            sender,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            outbound,
            crate::input::new_subscribers(),
        );

        let unknown = MailboxId(0xDEADBEEF_u64);
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
            .try_register_closure("client", Arc::new(|_, _, _, _, _, _| {}))
            .expect("register client mailbox");

        let store = Arc::new(crate::handle_store::HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        // Deliberately no `with_outbound` — exercises the local warn-drop path.

        let ctx = ComponentCtx::new(
            sender,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            outbound,
            crate::input::new_subscribers(),
        );

        ctx.send(
            MailboxId(0xDEADBEEF_u64),
            aether_data::KindId(0xABCD),
            vec![],
            0,
        );
        assert!(
            outbound_rx.try_recv().is_err(),
            "no bubble-up without a wired outbound"
        );
    }
}
