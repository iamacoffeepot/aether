// A loaded WASM component: its wasmtime `Store<SubstrateCtx>`, instance,
// and the cached handles needed to deliver mail. Milestone 1 uses a
// static-offset convention (mail payload written at `MAIL_OFFSET`) to
// match the spike; a guest-side allocator is future work per issue #18.

use aether_hub_protocol::SessionToken;
use wasmtime::{Engine, Linker, Memory, Module, Store, TypedFunc};

use crate::ctx::{StateBundle, SubstrateCtx};
use crate::mail::Mail;
use crate::sender_table::{SENDER_NONE, SenderEntry};

const MAIL_OFFSET: u32 = 1024;

/// Offset the substrate writes prior-state bytes to before calling
/// `on_rehydrate` (ADR-0016 §3). Deliberately separated from
/// `MAIL_OFFSET` so the two scratch regions don't overlap in the
/// worst-case size. The lifetimes are also disjoint in practice —
/// rehydrate runs once, post-init, before any mail arrives — but the
/// offset split keeps out-of-bounds checks obvious.
const STATE_OFFSET: u32 = 8192;

/// Contract with the guest: it exports a
/// `receive(kind, ptr, count, sender) -> u32` entrypoint and a `memory`
/// named `memory`. ADR-0013 widened the receive ABI with a fourth
/// `sender: u32` parameter — a per-instance handle the guest can pass
/// back to `reply_mail`, or `SENDER_NONE` for component-originated
/// mail. ADR-0015 adds optional `on_replace`, `on_drop`, and
/// `on_rehydrate` exports; the substrate calls them at the right
/// lifecycle moments when present and silently skips when absent
/// (no-op trait defaults compile down to no symbol under LTO, so
/// components that don't override stay backwards-compat).
pub struct Component {
    store: Store<SubstrateCtx>,
    memory: Memory,
    receive: TypedFunc<(u32, u32, u32, u32), u32>,
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
            instance.get_typed_func::<(u32, u32, u32, u32), u32>(&mut store, "receive")?;

        // Optional `init() -> u32` export: called once before the first
        // `receive`, used for one-shot bootstrap like resolving kind
        // names to ids. Per ADR-0005's registry-at-init flow.
        if let Ok(init) = instance.get_typed_func::<(), u32>(&mut store, "init") {
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
            .get_typed_func::<(u32, u32, u32), u32>(&mut store, "on_rehydrate")
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
    /// the per-instance `SenderTable` for every inbound that has a
    /// meaningful reply target — a Claude session (non-NIL
    /// `SessionToken`) or another component (`from_component`
    /// populated by `SubstrateCtx::send`). Broadcast-origin and
    /// system-generated mail pass `SENDER_NONE` so the guest's
    /// `mail.sender()` accessor returns `None`.
    pub fn deliver(&mut self, mail: &Mail) -> wasmtime::Result<u32> {
        let entry = if mail.sender != SessionToken::NIL {
            Some(SenderEntry::Session(mail.sender))
        } else {
            mail.from_component.map(SenderEntry::Component)
        };
        let handle = match entry {
            Some(e) => self.store.data_mut().sender_table.allocate(e),
            None => SENDER_NONE,
        };
        self.memory
            .write(&mut self.store, MAIL_OFFSET as usize, &mail.payload)?;
        self.receive.call(
            &mut self.store,
            (mail.kind, MAIL_OFFSET, mail.count, handle),
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
            eprintln!("substrate: on_replace hook trapped: {e}");
        }
    }

    /// Invoke the guest's `on_drop` hook if it exports one. Same trap
    /// containment as `on_replace`.
    pub fn on_drop(&mut self) {
        if let Some(f) = self.on_drop.clone()
            && let Err(e) = f.call(&mut self.store, ())
        {
            eprintln!("substrate: on_drop hook trapped: {e}");
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
    use crate::hub_client::HubOutbound;
    use crate::mail::MailboxId;
    use crate::queue::MailQueue;
    use crate::registry::Registry;

    fn ctx() -> SubstrateCtx {
        SubstrateCtx::new(
            MailboxId(0),
            Arc::new(Registry::new()),
            Arc::new(MailQueue::new()),
            HubOutbound::disconnected(),
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
            (func (export "receive") (param i32 i32 i32 i32) (result i32)
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
            (func (export "receive") (param i32 i32 i32 i32) (result i32)
                i32.const 0))
    "#;

    const WAT_TRAP_ON_DROP: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive") (param i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_drop") (result i32)
                unreachable))
    "#;

    /// ADR-0016 save-side: `on_replace` calls `save_state` with a
    /// version and 4 bytes at offset 300 (`0xDE 0xAD 0xBE 0xEF`).
    const WAT_SAVES_STATE: &str = r#"
        (module
            (import "aether" "save_state"
                (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 300) "\de\ad\be\ef")
            (func (export "receive") (param i32 i32 i32 i32) (result i32)
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
            (import "aether" "save_state"
                (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive") (param i32 i32 i32 i32) (result i32)
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
            (func (export "receive") (param i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_rehydrate") (param i32 i32 i32) (result i32)
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
            (func (export "receive") (param i32 i32 i32 i32) (result i32)
                i32.const 500
                local.get 3
                i32.store
                i32.const 0))
    "#;

    /// ADR-0013: `receive` echoes a reply back to the sender under
    /// whatever kind id the caller registered. Payload is empty —
    /// the round-trip is the observable behavior.
    const WAT_REPLIES: &str = r#"
        (module
            (import "aether" "reply_mail"
                (func $reply_mail (param i32 i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive") (param i32 i32 i32 i32) (result i32)
                (drop (call $reply_mail
                    (local.get 3) ;; sender handle from receive param
                    (i32.const 0) ;; kind id 0 — registered in the test
                    (i32.const 0) ;; ptr
                    (i32.const 0) ;; len
                    (i32.const 1))) ;; count
                i32.const 0))
    "#;

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
        use crate::sender_table::SENDER_NONE;

        let mut component = instantiate(WAT_STORES_SENDER);
        // Mail::new defaults sender to SessionToken::NIL.
        let mail = SubstrateMail::new(M(0), 0, vec![], 1);
        component.deliver(&mail).expect("deliver");
        assert_eq!(component.read_u32(500), SENDER_NONE);
    }

    #[test]
    fn deliver_with_real_token_allocates_session_handle() {
        use aether_hub_protocol::{SessionToken, Uuid};

        use crate::mail::{Mail as SubstrateMail, MailboxId as M};
        use crate::sender_table::{SENDER_NONE, SenderEntry};

        let mut component = instantiate(WAT_STORES_SENDER);
        let token = SessionToken(Uuid::from_u128(0xaaaa));
        let mail = SubstrateMail::new(M(0), 0, vec![], 1).with_sender(token);
        component.deliver(&mail).expect("deliver");
        let observed = component.read_u32(500);
        assert_ne!(observed, SENDER_NONE);
        assert_eq!(
            component.store.data().sender_table.resolve(observed),
            Some(SenderEntry::Session(token)),
        );
    }

    #[test]
    fn deliver_with_from_component_allocates_component_handle() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};
        use crate::sender_table::{SENDER_NONE, SenderEntry};

        let mut component = instantiate(WAT_STORES_SENDER);
        // ADR-0017: component-origin mail (no session token, but a
        // populated `from_component`) gets a Component-variant handle.
        let mail = SubstrateMail::new(M(0), 0, vec![], 1).with_origin(M(7));
        component.deliver(&mail).expect("deliver");
        let observed = component.read_u32(500);
        assert_ne!(observed, SENDER_NONE);
        assert_eq!(
            component.store.data().sender_table.resolve(observed),
            Some(SenderEntry::Component(M(7))),
        );
    }

    #[test]
    fn deliver_session_takes_priority_over_component_origin() {
        // If both a session token and a from_component are set (which
        // can happen if hub-originated mail somehow gets re-routed
        // through SubstrateCtx::send), the Session variant wins. The
        // session is the more specific reply target.
        use aether_hub_protocol::{SessionToken, Uuid};

        use crate::mail::{Mail as SubstrateMail, MailboxId as M};
        use crate::sender_table::SenderEntry;

        let mut component = instantiate(WAT_STORES_SENDER);
        let token = SessionToken(Uuid::from_u128(0xbbbb));
        let mail = SubstrateMail::new(M(0), 0, vec![], 1)
            .with_sender(token)
            .with_origin(M(99));
        component.deliver(&mail).expect("deliver");
        let observed = component.read_u32(500);
        match component.store.data().sender_table.resolve(observed) {
            Some(SenderEntry::Session(t)) => assert_eq!(t, token),
            other => panic!("expected Session, got {other:?}"),
        }
    }

    fn plane_ctx_for_reply() -> (
        SubstrateCtx,
        std::sync::mpsc::Receiver<aether_hub_protocol::EngineToHub>,
    ) {
        use aether_hub_protocol::{KindDescriptor, KindEncoding};

        use crate::hub_client::HubOutbound;
        use crate::mail::MailboxId as M;

        let (outbound, rx) = HubOutbound::test_channel();
        let registry = Arc::new(Registry::new());
        // Kind id 0 is what `WAT_REPLIES` passes to reply_mail.
        registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "test.pong".into(),
                encoding: KindEncoding::Signal,
            })
            .expect("register kind");
        let ctx = SubstrateCtx::new(M(0), registry, Arc::new(MailQueue::new()), outbound);
        (ctx, rx)
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

        use crate::mail::{Mail as SubstrateMail, MailboxId as M};

        let (ctx, rx) = plane_ctx_for_reply();
        let mut component = instantiate_with_ctx(WAT_REPLIES, ctx);

        let token = SessionToken(Uuid::from_u128(0xbeef));
        let mail = SubstrateMail::new(M(0), 0, vec![], 1).with_sender(token);
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

        let (ctx, rx) = plane_ctx_for_reply();
        let mut component = instantiate_with_ctx(WAT_REPLIES, ctx);

        // NIL sender → SENDER_NONE reaches the guest → reply_mail
        // returns REPLY_UNKNOWN_HANDLE and outbound stays quiet.
        let mail = SubstrateMail::new(M(0), 0, vec![], 1);
        component.deliver(&mail).expect("deliver");
        assert!(rx.try_recv().is_err(), "no frame should have been sent");
    }

    /// ADR-0017 component-reply path. Builds a ctx whose registry
    /// has both a fake originating-component mailbox (id 1, name
    /// "caller") and a "test.pong" kind. When `WAT_REPLIES` calls
    /// `reply_mail` with the Component-variant handle the substrate
    /// allocated, the reply lands on the local `MailQueue` —
    /// outbound stays empty.
    fn plane_ctx_with_caller_mailbox() -> (
        SubstrateCtx,
        std::sync::mpsc::Receiver<aether_hub_protocol::EngineToHub>,
        Arc<MailQueue>,
        crate::mail::MailboxId,
    ) {
        use aether_hub_protocol::{KindDescriptor, KindEncoding};

        use crate::hub_client::HubOutbound;
        use crate::mail::MailboxId as M;

        let (outbound, rx) = HubOutbound::test_channel();
        let registry = Arc::new(Registry::new());
        let caller = registry.register_component("caller");
        registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "test.pong".into(),
                encoding: KindEncoding::Signal,
            })
            .expect("register kind");
        let queue = Arc::new(MailQueue::new());
        let ctx = SubstrateCtx::new(M(0), registry, Arc::clone(&queue), outbound);
        (ctx, rx, queue, caller)
    }

    #[test]
    fn reply_mail_to_component_enqueues_on_mailqueue() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};

        let (ctx, rx_outbound, queue, caller) = plane_ctx_with_caller_mailbox();
        let mut component = instantiate_with_ctx(WAT_REPLIES, ctx);

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
        assert_eq!(queued.kind, 0);
    }

    #[test]
    fn reply_mail_to_dropped_component_silently_discards() {
        use crate::mail::{Mail as SubstrateMail, MailboxId as M};

        let (ctx, rx_outbound, queue, caller) = plane_ctx_with_caller_mailbox();
        // Drop the caller before the reply lands. SubstrateCtx::send
        // logs and discards rather than panicking — the inbound flow
        // still completes successfully from the receiving guest's
        // perspective.
        ctx.registry.drop_mailbox(caller).expect("drop caller");
        let mut component = instantiate_with_ctx(WAT_REPLIES, ctx);

        let mail = SubstrateMail::new(M(0), 0, vec![], 1).with_origin(caller);
        component.deliver(&mail).expect("deliver");

        assert!(rx_outbound.try_recv().is_err());
        // Queue stays empty — dropped mailbox path discards.
        assert!(
            queue.try_pop().is_none(),
            "reply to dropped mailbox must not enqueue"
        );
    }
}
