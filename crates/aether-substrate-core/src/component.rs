// A loaded WASM component: its wasmtime `Store<SubstrateCtx>`, instance,
// and the cached handles needed to deliver mail. Mail payloads are
// written to the guest at a static `MAIL_OFFSET`; a guest-side
// allocator is parked until an actual use case forces the question.

use std::sync::mpsc::Receiver;

use wasmtime::{Engine, Linker, Memory, Module, Store, TypedFunc};

use crate::ctx::{StateBundle, SubstrateCtx};
use crate::mail::{Mail, ReplyTarget};
use crate::reply_table::{NO_REPLY_HANDLE, ReplyEntry};

const MAIL_OFFSET: u32 = 1024;

/// Sentinel the ADR-0033 `#[handlers]` dispatcher returns from
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
    store: Store<SubstrateCtx>,
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
        linker: &Linker<SubstrateCtx>,
        module: &Module,
        ctx: SubstrateCtx,
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
        let mailbox_id = store.data().sender.0;
        if let Ok(init) = instance.get_typed_func::<u64, u32>(&mut store, "init") {
            init.call(&mut store, mailbox_id)?;
        } else if let Ok(init) = instance.get_typed_func::<(), u32>(&mut store, "init") {
            init.call(&mut store, ())?;
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
    /// `SessionToken`) or another component (`from_component`
    /// populated by `SubstrateCtx::send`). Broadcast-origin and
    /// system-generated mail pass `NO_REPLY_HANDLE` so the guest's
    /// `mail.reply_to()` accessor returns `None`.
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
            // Component-variant reply_to reaches a real component's
            // `deliver` only via the mailer-routed reply path (the
            // sink replied to a local component): the reply itself
            // has no one to reply back to, so the guest sees no
            // `reply_to`. Falls through to the `from_component`
            // check, matching the None path.
            ReplyTarget::None | ReplyTarget::Component(_) => mail
                .from_component
                .map(|m| ReplyEntry::new(ReplyTarget::Component(m), correlation)),
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
            (mail.kind, MAIL_OFFSET, byte_len, mail.count, handle),
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

    /// Install the mpsc `Receiver` the dispatcher will read from
    /// (ADR-0042). Called by `ComponentEntry::spawn` right after the
    /// mpsc pair is built; the host fn for `wait_reply_p32` later
    /// drains the same receiver when a guest parks on a reply.
    pub fn install_inbox_rx(&mut self, rx: Receiver<Mail>) {
        self.store.data().install_inbox_rx(rx);
    }

    /// Pop the next mail for the dispatcher. Drains the overflow
    /// buffer first (FIFO-preserved mail that a completed
    /// `wait_reply_p32` set aside), then reads from the mpsc
    /// receiver installed via `install_inbox_rx`. `None` means both
    /// are empty and the inbox has been disconnected — the
    /// dispatcher takes that as its exit signal.
    pub fn next_mail(&mut self) -> Option<Mail> {
        self.store.data().next_mail()
    }

    /// Push a mail onto this component's overflow buffer directly.
    /// Test-only: lets unit tests seed the overflow and observe that
    /// `next_mail` drains it before the mpsc. Production code only
    /// feeds the overflow through `wait_reply_p32`'s drain path.
    #[cfg(test)]
    pub fn push_overflow_for_test(&mut self, mail: Mail) {
        self.store
            .data()
            .inbox_overflow
            .lock()
            .unwrap()
            .push_back(mail);
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
    use crate::hub_client::HubOutbound;
    use crate::mail::MailboxId;
    use crate::mailer::Mailer;
    use crate::registry::Registry;

    fn ctx() -> SubstrateCtx {
        SubstrateCtx::new(
            MailboxId(0),
            Arc::new(Registry::new()),
            Arc::new(Mailer::new()),
            HubOutbound::disconnected(),
            crate::input::new_subscribers(),
        )
    }

    fn instantiate(wat: &str) -> Component {
        let engine = Engine::default();
        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        crate::host_fns::register(&mut linker).expect("register host fns");
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
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};
        use crate::reply_table::NO_REPLY_HANDLE;

        let mut component = instantiate(WAT_STORES_SENDER);
        // Mail::new defaults sender to SessionToken::NIL.
        let mail = SubstrateMail::new(M(0), 0, vec![], 1);
        component.deliver(&mail).expect("deliver");
        assert_eq!(component.read_u32(500), NO_REPLY_HANDLE);
    }

    #[test]
    fn deliver_with_real_token_allocates_session_handle() {
        use aether_hub_protocol::{SessionToken, Uuid};

        use crate::mail::{Mail as SubstrateMail, MailboxId as M, ReplyTarget, ReplyTo};
        use crate::reply_table::{NO_REPLY_HANDLE, ReplyEntry};

        let mut component = instantiate(WAT_STORES_SENDER);
        let token = SessionToken(Uuid::from_u128(0xaaaa));
        let mail = SubstrateMail::new(M(0), 0, vec![], 1)
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
    fn deliver_with_from_component_allocates_component_handle() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};
        use crate::reply_table::{NO_REPLY_HANDLE, ReplyEntry};

        let mut component = instantiate(WAT_STORES_SENDER);
        // ADR-0017: component-origin mail (no session token, but a
        // populated `from_component`) gets a Component-variant handle.
        let mail = SubstrateMail::new(M(0), 0, vec![], 1).with_origin(M(7));
        component.deliver(&mail).expect("deliver");
        let observed = component.read_u32(500);
        assert_ne!(observed, NO_REPLY_HANDLE);
        assert_eq!(
            component.store.data().reply_table.resolve(observed),
            Some(ReplyEntry::component(M(7))),
        );
    }

    #[test]
    fn deliver_session_takes_priority_over_component_origin() {
        // If both a session token and a from_component are set (which
        // can happen if hub-originated mail somehow gets re-routed
        // through SubstrateCtx::send), the Session variant wins. The
        // session is the more specific reply target.
        use aether_hub_protocol::{SessionToken, Uuid};

        use crate::mail::{Mail as SubstrateMail, MailboxId as M, ReplyTarget, ReplyTo};
        use crate::reply_table::ReplyEntry;

        let mut component = instantiate(WAT_STORES_SENDER);
        let token = SessionToken(Uuid::from_u128(0xbbbb));
        let mail = SubstrateMail::new(M(0), 0, vec![], 1)
            .with_reply_to(ReplyTo::to(ReplyTarget::Session(token)))
            .with_origin(M(99));
        component.deliver(&mail).expect("deliver");
        let observed = component.read_u32(500);
        match component.store.data().reply_table.resolve(observed) {
            Some(ReplyEntry {
                target: ReplyTarget::Session(t),
                ..
            }) => assert_eq!(t, token),
            other => panic!("expected Session, got {other:?}"),
        }
    }

    fn plane_ctx_for_reply() -> (
        SubstrateCtx,
        std::sync::mpsc::Receiver<aether_hub_protocol::EngineToHub>,
        u64,
    ) {
        use aether_hub_protocol::{KindDescriptor, SchemaType};

        use crate::hub_client::HubOutbound;
        use crate::mail::MailboxId as M;

        let (outbound, rx) = HubOutbound::test_channel();
        let registry = Arc::new(Registry::new());
        let pong_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "test.pong".into(),
                schema: SchemaType::Unit,
            })
            .expect("register kind");
        let ctx = SubstrateCtx::new(
            M(0),
            registry,
            Arc::new(Mailer::new()),
            outbound,
            crate::input::new_subscribers(),
        );
        (ctx, rx, pong_id)
    }

    fn instantiate_with_ctx(wat: &str, ctx: SubstrateCtx) -> Component {
        let engine = Engine::default();
        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        crate::host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(wat).unwrap();
        let module = Module::new(&engine, &wasm).unwrap();
        Component::instantiate(&engine, &linker, &module, ctx).unwrap()
    }

    #[test]
    fn reply_mail_emits_session_addressed_frame() {
        use aether_hub_protocol::{ClaudeAddress, EngineToHub, SessionToken, Uuid};

        use crate::mail::{Mail as SubstrateMail, MailboxId as M, ReplyTarget, ReplyTo};

        let (ctx, rx, pong_id) = plane_ctx_for_reply();
        let mut component = instantiate_with_ctx(&wat_replies(pong_id), ctx);

        let token = SessionToken(Uuid::from_u128(0xbeef));
        let mail = SubstrateMail::new(M(0), 0, vec![], 1)
            .with_reply_to(ReplyTo::to(ReplyTarget::Session(token)));
        component.deliver(&mail).expect("deliver");

        let frame = rx.try_recv().expect("outbound frame queued");
        let EngineToHub::Mail(mail_frame) = frame else {
            panic!("expected EngineToHub::Mail, got {frame:?}");
        };
        assert_eq!(mail_frame.address, ClaudeAddress::Session(token));
        assert_eq!(mail_frame.kind_name, "test.pong");
    }

    #[test]
    fn reply_mail_with_unknown_handle_sends_no_frame() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};

        let (ctx, rx, pong_id) = plane_ctx_for_reply();
        let mut component = instantiate_with_ctx(&wat_replies(pong_id), ctx);

        // NIL sender → NO_REPLY_HANDLE reaches the guest → reply_mail
        // returns REPLY_UNKNOWN_HANDLE and outbound stays quiet.
        let mail = SubstrateMail::new(M(0), 0, vec![], 1);
        component.deliver(&mail).expect("deliver");
        assert!(rx.try_recv().is_err(), "no frame should have been sent");
    }

    /// ADR-0017 component-reply path. Builds a ctx whose registry
    /// has both a fake originating-component mailbox (id 1, name
    /// "caller") and a "test.pong" kind. When `WAT_REPLIES` calls
    /// `reply_mail` with the Component-variant handle the substrate
    /// allocated, the reply lands on the local `Mailer` —
    /// outbound stays empty.
    #[allow(dead_code)]
    fn plane_ctx_with_caller_mailbox() -> (
        SubstrateCtx,
        std::sync::mpsc::Receiver<aether_hub_protocol::EngineToHub>,
        Arc<Mailer>,
        crate::mail::MailboxId,
        u64,
    ) {
        use aether_hub_protocol::{KindDescriptor, SchemaType};

        use crate::hub_client::HubOutbound;
        use crate::mail::MailboxId as M;

        let (outbound, rx) = HubOutbound::test_channel();
        let registry = Arc::new(Registry::new());
        let caller = registry.register_component("caller");
        let pong_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "test.pong".into(),
                schema: SchemaType::Unit,
            })
            .expect("register kind");
        let queue = Arc::new(Mailer::new());
        let ctx = SubstrateCtx::new(
            M(0),
            registry,
            Arc::clone(&queue),
            outbound,
            crate::input::new_subscribers(),
        );
        (ctx, rx, queue, caller, pong_id)
    }

    // Retired under ADR-0038 Phase 2: these two tests peeked at
    // `Mailer::try_pop` to observe reply-mail routing, but Phase 2
    // retired the queue's inspectable deque in favour of inline
    // routing directly into per-component inboxes. The component-
    // reply path (ADR-0017) is still covered end-to-end by the MCP
    // harness; the boundary assertion here would require spinning up
    // a full dispatcher for the `caller` mailbox just to observe, so
    // the lower-level coverage retires with the old storage.
    #[cfg(any())]
    #[test]
    fn reply_mail_to_component_enqueues_on_mailqueue() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};

        let (ctx, rx_outbound, queue, caller, pong_id) = plane_ctx_with_caller_mailbox();
        let mut component = instantiate_with_ctx(&wat_replies(pong_id), ctx);

        // Inbound mail from "caller" — substrate allocates a
        // Component-variant handle. The guest's reply_mail call
        // routes through SubstrateCtx::send to the local queue.
        let mail = SubstrateMail::new(M(0), 0, vec![], 1).with_origin(caller);
        component.deliver(&mail).expect("deliver");

        // Outbound stayed quiet — no hub frame for component replies.
        assert!(
            rx_outbound.try_recv().is_err(),
            "component reply must not hit HubOutbound"
        );
        // The local queue picked up the reply addressed to caller.
        let queued = queue.try_pop().expect("reply enqueued");
        assert_eq!(queued.recipient, caller);
        assert_eq!(queued.kind, pong_id);
    }

    #[cfg(any())]
    #[test]
    fn reply_mail_to_dropped_component_silently_discards() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};

        let (ctx, rx_outbound, queue, caller, pong_id) = plane_ctx_with_caller_mailbox();
        // Drop the caller before the reply lands. SubstrateCtx::send
        // logs and discards rather than panicking — the inbound flow
        // still completes successfully from the receiving guest's
        // perspective.
        ctx.registry.drop_mailbox(caller).expect("drop caller");
        let mut component = instantiate_with_ctx(&wat_replies(pong_id), ctx);

        let mail = SubstrateMail::new(M(0), 0, vec![], 1).with_origin(caller);
        component.deliver(&mail).expect("deliver");

        assert!(rx_outbound.try_recv().is_err());
        // Queue stays empty — dropped mailbox path discards.
        assert!(
            queue.try_pop().is_none(),
            "reply to dropped mailbox must not enqueue"
        );
    }

    /// ADR-0042 `wait_reply_p32` host fn. The guest expects a reply
    /// of kind `0xAAAA_AAAA_AAAA_AAAA`, writes a maximum of 64 bytes
    /// starting at offset 600, and uses the mail's `count` field as
    /// the `timeout_ms` argument so tests can vary it per invocation.
    /// The host fn's return value (bytes written, or a negative
    /// sentinel) lands at offset 700.
    const WAT_SYNC_WAIT: &str = r#"
        (module
            (import "aether" "wait_reply_p32"
                (func $wait (param i64 i32 i32 i32 i64) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 700
                (call $wait
                    (i64.const -6148914691236517206) ;; 0xAAAA_AAAA_AAAA_AAAA reinterpreted as i64
                    (i32.const 600)                  ;; out_ptr
                    (i32.const 64)                   ;; out_cap
                    (local.get 3)                    ;; timeout_ms = count (param 3 after byte_len shifted in at 2)
                    (i64.const 0))                   ;; expected_correlation = 0 (kind-only filter)
                i32.store
                i32.const 0))
    "#;

    const SYNC_WAIT_EXPECTED_KIND: u64 = 0xAAAA_AAAA_AAAA_AAAA;

    /// Helper: build a trigger mail whose `count` field carries the
    /// timeout_ms argument into the WAT. The `kind` is irrelevant —
    /// the guest ignores it and passes a hardcoded `expected_kind` to
    /// the host fn.
    fn trigger_wait(timeout_ms: u32) -> Mail {
        use crate::mail::MailboxId;
        Mail::new(MailboxId(0), 0xBEEF, vec![], timeout_ms)
    }

    /// Build a sync-wait component with an installed inbox so
    /// `wait_reply_p32` has something to drain. Returns the
    /// component + the `Sender<Mail>` the test can push matching /
    /// non-matching mail through; drop the Sender to trigger
    /// disconnect.
    fn instantiate_sync_wait_with_inbox() -> (Component, std::sync::mpsc::Sender<Mail>) {
        use std::sync::mpsc;

        use wasmtime::{Engine, Linker, Module};

        let engine = Engine::default();
        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        crate::host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(WAT_SYNC_WAIT).expect("compile WAT");
        let module = Module::new(&engine, &wasm).expect("compile module");
        let mut component =
            Component::instantiate(&engine, &linker, &module, ctx()).expect("instantiate");
        let (tx, rx) = mpsc::channel::<Mail>();
        component.install_inbox_rx(rx);
        (component, tx)
    }

    #[test]
    fn wait_reply_returns_timeout_when_no_match_arrives() {
        let (mut component, _tx) = instantiate_sync_wait_with_inbox();
        component.deliver(&trigger_wait(10)).expect("deliver");

        let result = component.read_u32(700) as i32;
        assert_eq!(
            result,
            crate::host_fns::WAIT_TIMEOUT,
            "no matching mail pushed — expected WAIT_TIMEOUT",
        );
    }

    #[test]
    fn wait_reply_poll_mode_returns_timeout_immediately() {
        // timeout_ms = 0 → try_recv path; no pre-queued mail → -1.
        let (mut component, _tx) = instantiate_sync_wait_with_inbox();
        component.deliver(&trigger_wait(0)).expect("deliver");

        let result = component.read_u32(700) as i32;
        assert_eq!(result, crate::host_fns::WAIT_TIMEOUT);
    }

    #[test]
    fn wait_reply_writes_payload_and_returns_byte_count_on_match() {
        use std::thread;
        use std::time::Duration;

        use crate::host_fns;
        use crate::mail::MailboxId as M;

        let (component, tx) = instantiate_sync_wait_with_inbox();

        // Deliver on a worker so we can push matching mail from
        // this thread while the guest is parked inside the host
        // fn's drain loop.
        let handle = thread::spawn(move || {
            let mut component = component;
            component.deliver(&trigger_wait(1000)).expect("deliver");
            component
        });

        // Nudge past the host fn's entry so the drain is already
        // parked on recv_timeout before our mail hits the mpsc.
        thread::sleep(Duration::from_millis(50));

        let payload = vec![1, 2, 3, 4, 5];
        tx.send(Mail::new(M(0), SYNC_WAIT_EXPECTED_KIND, payload.clone(), 1))
            .expect("send matching mail");

        let mut component = handle.join().expect("worker panicked");
        let result = component.read_u32(700) as i32;
        assert_eq!(result, payload.len() as i32, "byte count returned");
        assert_eq!(
            component.read_bytes(600, payload.len()),
            payload,
            "payload copied into guest memory at out_ptr",
        );
        let _ = host_fns::WAIT_TIMEOUT; // keep import live without warning
    }

    #[test]
    fn wait_reply_buffers_non_matching_mail_into_overflow() {
        // ADR-0042 drain loop: a non-matching mail arriving during
        // the wait must end up on the overflow buffer so the
        // dispatcher serves it ahead of new mpsc mail afterwards.
        use std::thread;
        use std::time::Duration;

        use crate::mail::MailboxId as M;

        let (component, tx) = instantiate_sync_wait_with_inbox();
        let handle = thread::spawn(move || {
            let mut component = component;
            component.deliver(&trigger_wait(1000)).expect("deliver");
            component
        });

        thread::sleep(Duration::from_millis(50));

        // Non-match first; then the real reply.
        tx.send(Mail::new(M(0), 0xDEAD_BEEF, vec![42], 1))
            .expect("send non-match");
        tx.send(Mail::new(M(0), SYNC_WAIT_EXPECTED_KIND, vec![7], 1))
            .expect("send match");

        let component = handle.join().expect("worker panicked");
        // Match drained cleanly; the non-match should now be sitting
        // in overflow, waiting for the dispatcher to pick it up.
        let overflow_len = component.store.data().inbox_overflow.lock().unwrap().len();
        assert_eq!(overflow_len, 1, "non-match mail should be buffered");
    }

    #[test]
    fn wait_reply_returns_buffer_too_small_when_payload_exceeds_cap() {
        use std::thread;
        use std::time::Duration;

        use crate::host_fns;
        use crate::mail::MailboxId as M;

        let (component, tx) = instantiate_sync_wait_with_inbox();
        let handle = thread::spawn(move || {
            let mut component = component;
            component.deliver(&trigger_wait(1000)).expect("deliver");
            component
        });

        thread::sleep(Duration::from_millis(50));

        // WAT's out_cap is 64; push a 128-byte payload.
        let payload = vec![0x7Fu8; 128];
        tx.send(Mail::new(M(0), SYNC_WAIT_EXPECTED_KIND, payload, 1))
            .expect("send matching-but-too-big mail");

        let mut component = handle.join().expect("worker panicked");
        let result = component.read_u32(700) as i32;
        assert_eq!(result, host_fns::WAIT_BUFFER_TOO_SMALL);
    }

    /// ADR-0042 correlation: a sync wait filters on
    /// `expected_correlation` so it picks *its own* reply out of
    /// the inbox rather than the first `ReadResult`-kind mail that
    /// happens to be queued. Regression guard — without the
    /// correlation filter, a stale prior reply of the same kind
    /// would be consumed as if it were the one we're waiting on.
    #[test]
    fn wait_reply_filters_by_correlation_not_just_kind() {
        use std::sync::mpsc;

        use wasmtime::{Engine, Linker, Module};

        use crate::mail::{MailboxId as M, ReplyTarget, ReplyTo};

        // WAT that waits on kind `SYNC_WAIT_EXPECTED_KIND` with
        // expected_correlation = 42, timeout = 1s. Stores payload
        // byte count at offset 700.
        const WAT: &str = r#"
            (module
                (import "aether" "wait_reply_p32"
                    (func $wait (param i64 i32 i32 i32 i64) (result i32)))
                (memory (export "memory") 1)
                (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                    i32.const 700
                    (call $wait
                        (i64.const -6148914691236517206) ;; expected_kind = 0xAAAA...
                        (i32.const 600)                  ;; out_ptr
                        (i32.const 64)                   ;; out_cap
                        (i32.const 1000)                 ;; timeout_ms
                        (i64.const 42))                  ;; expected_correlation
                    i32.store
                    i32.const 0))
        "#;

        let engine = Engine::default();
        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        crate::host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(WAT).expect("compile WAT");
        let module = Module::new(&engine, &wasm).expect("compile module");
        let mut component =
            Component::instantiate(&engine, &linker, &module, ctx()).expect("instantiate");
        let (tx, rx) = mpsc::channel::<Mail>();
        component.install_inbox_rx(rx);

        // Pre-queue a decoy reply: same kind, different correlation.
        // Without the correlation filter the sync wait would consume
        // THIS one (first-in) as if it were the one we're waiting on.
        let decoy_payload = vec![0xDE, 0xAD];
        tx.send(
            Mail::new(M(0), SYNC_WAIT_EXPECTED_KIND, decoy_payload, 1)
                .with_reply_to(ReplyTo::with_correlation(ReplyTarget::None, 10)),
        )
        .expect("send decoy");

        // Now the reply we're actually waiting on: correlation = 42.
        let real_payload = vec![1, 2, 3, 4, 5];
        tx.send(
            Mail::new(M(0), SYNC_WAIT_EXPECTED_KIND, real_payload.clone(), 1)
                .with_reply_to(ReplyTo::with_correlation(ReplyTarget::None, 42)),
        )
        .expect("send real reply");

        component.deliver(&trigger_wait(0)).expect("deliver");

        let result = component.read_u32(700) as i32;
        assert_eq!(
            result,
            real_payload.len() as i32,
            "expected byte count for the correlation-42 reply"
        );
        assert_eq!(
            component.read_bytes(600, real_payload.len()),
            real_payload,
            "host fn copied the wrong payload — decoy reply consumed instead of real one",
        );
        // Decoy should still be sitting in overflow — the dispatcher
        // would deliver it on its next pass after the wait returned.
        let overflow = component.store.data().inbox_overflow.lock().unwrap();
        assert_eq!(overflow.len(), 1, "decoy mail should be in overflow");
        assert_eq!(overflow[0].reply_to.correlation_id, 10);
    }

    #[test]
    fn wait_reply_returns_cancelled_when_sender_drops() {
        use std::thread;
        use std::time::Duration;

        use crate::host_fns;

        let (component, tx) = instantiate_sync_wait_with_inbox();
        let handle = thread::spawn(move || {
            let mut component = component;
            component.deliver(&trigger_wait(1000)).expect("deliver");
            component
        });

        thread::sleep(Duration::from_millis(50));
        // Teardown-style cancellation under the drain+buffer design
        // is "drop the mpsc Sender" — the host fn's recv_timeout
        // wakes with Disconnected.
        drop(tx);

        let mut component = handle.join().expect("worker panicked");
        let result = component.read_u32(700) as i32;
        assert_eq!(result, host_fns::WAIT_CANCELLED);
    }
}
