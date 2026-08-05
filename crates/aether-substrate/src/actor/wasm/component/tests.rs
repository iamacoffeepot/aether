// Tests hold the capture `Mutex` guard across the assertion block so
// the snapshot reads atomically against the concurrent push path.
#![allow(clippy::significant_drop_tightening)]
#![allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "these tests exercise the rendered-path resolution machinery itself — mailbox_id_from_path is the surface under test"
)]
use std::sync::Arc;

use wasmtime::{Engine, Linker, Module};

use std::sync::Mutex;

use super::*;
use crate::actor::native::NativeBinding;
use crate::actor::wasm::host_fns;
use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::{EgressEvent, HubOutbound};
use crate::mail::registry;
use crate::mail::registry::InboxHandler;
use crate::mail::registry::OwnedDispatch;
use crate::mail::registry::Registry;
use crate::mail::registry::RegistryOwnerLease;
use crate::mail::registry::effect::{EffectBatch, PreparedAliasRoute, RegistryEffect};
use crate::mail::{Mail, MailId, MailboxId};
use crate::scheduler::WakeSink;
use crate::testing::boot_authority;
use aether_data::tagged_id::Tag;
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// Captured `(mail_id, root, parent_mail)` triple for the
/// lineage-propagation tests in this module.
type LineageCapture = Arc<Mutex<Vec<(MailId, MailId, Option<MailId>)>>>;

/// Build an inbox handler that captures every dispatched mail's lineage
/// triple into a shared `Vec`, discharging each dispatch (ADR-0094:
/// terminal test capture sink). Returns the capture handle alongside.
fn lineage_capture_handler() -> (LineageCapture, Arc<dyn InboxHandler>) {
    let captured: LineageCapture = Arc::new(Mutex::new(Vec::new()));
    let captured_for_handler = Arc::clone(&captured);
    let handler = Arc::new(move |dispatch: OwnedDispatch| {
        dispatch.discharge();
        captured_for_handler.lock().unwrap().push((dispatch.mail_id, dispatch.root, dispatch.parent_mail));
    });
    (captured, handler)
}

/// Register a sink that captures every dispatched mail's lineage
/// triple into a shared `Vec`. Both lineage tests below share
/// this setup; the helper returns the capture handle and the
/// registered mailbox id.
fn register_lineage_capture_sink(registry: &Arc<Registry>, name: &str) -> (LineageCapture, MailboxId) {
    let (captured, handler) = lineage_capture_handler();
    let sink_id = registry.try_register_inbox(&boot_authority(), name, handler).expect("register sink");
    (captured, sink_id)
}

fn ctx() -> ComponentCtx {
    let registry = Arc::new(Registry::new());
    ComponentCtx::new(MailboxId(0), Arc::clone(&registry), Arc::new(Mailer::new(registry)), HubOutbound::disconnected())
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

fn ctx_with_parent(sender: MailboxId, parent: MailboxId) -> ComponentCtx {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let mut ctx = ComponentCtx::new(sender, registry, Arc::clone(&mailer), HubOutbound::disconnected());
    ctx.install_binding(Arc::new(NativeBinding::new_for_test_with_parent(mailer, sender, parent)));
    ctx
}

fn replacement_ctx_pair(sender: MailboxId, parent: MailboxId) -> (ComponentCtx, ComponentCtx) {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), sender, parent));
    let build = || {
        let mut ctx =
            ComponentCtx::new(sender, Arc::clone(&registry), Arc::clone(&mailer), HubOutbound::disconnected());
        ctx.install_binding(Arc::clone(&binding));
        ctx
    };
    (build(), build())
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

fn wat_init_with_parent() -> String {
    format!(
        r#"
        (module
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "init_with_parent_p32") (param i64 i64 i32 i32) (result i32)
                i32.const 200
                local.get 0
                i32.wrap_i64
                i32.store
                i32.const 204
                local.get 1
                i32.wrap_i64
                i32.store
                i32.const 0))
    "#
    )
}

fn wat_typed_init(export: &str, parent_aware: bool) -> String {
    let init = if parent_aware {
        format!(
            r#"
            (func (export "{export}") (param i64 i64 i64 i32 i32) (result i32)
                i32.const 204
                local.get 1
                i32.wrap_i64
                i32.store
                i32.const 208
                local.get 2
                i32.wrap_i64
                i32.store
                i32.const 0)"#
        )
    } else {
        format!(
            r#"
            (func (export "{export}") (param i64 i64 i32 i32) (result i32)
                i32.const 208
                local.get 1
                i32.wrap_i64
                i32.store
                i32.const 0)"#
        )
    };
    format!(
        r#"
        (module
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            {init})
    "#
    )
}

fn instantiate_typed_with_ctx(wat: &str, ctx: ComponentCtx, type_tag: u64) -> Component {
    let engine = Engine::default();
    let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
    host_fns::register(&mut linker).expect("register host fns");
    let wasm = wat::parse_str(wat).expect("compile WAT");
    let module = Module::new(&engine, &wasm).expect("compile module");
    Component::instantiate(&engine, &linker, &module, ctx, &[], Some(type_tag)).expect("instantiate typed")
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

#[test]
fn parent_aware_init_receives_parent_on_initial_and_replacement_instantiation() {
    let sender = MailboxId(0x4c01);
    let parent = MailboxId(0x4c02);
    let wat = wat_init_with_parent();
    let (initial_ctx, replacement_ctx) = replacement_ctx_pair(sender, parent);

    let mut initial = instantiate_with_ctx(&wat, initial_ctx);
    assert_eq!(initial.read_u32(200), 0x4c01);
    assert_eq!(initial.read_u32(204), 0x4c02);

    // `replace_component` creates a fresh `ComponentCtx`, reinstalls the same
    // native binding, and calls this same instantiate path.
    let mut replacement = instantiate_with_ctx(&wat, replacement_ctx);
    assert_eq!(replacement.read_u32(204), 0x4c02);
}

#[test]
fn legacy_config_init_still_loads_when_the_binding_has_a_parent() {
    let sender = MailboxId(0x4c11);
    let parent = MailboxId(0x4c12);

    let mut component = instantiate_with_ctx(&wat_init_with_config(), ctx_with_parent(sender, parent));

    assert_eq!(component.read_u32(200), 0x4c11);
}

#[test]
fn typed_parent_aware_init_receives_parent_and_type_tag() {
    let sender = MailboxId(0x4c21);
    let parent = MailboxId(0x4c22);
    let type_tag = 0x4c23;
    let wat = wat_typed_init("init_typed_with_parent_p32", true);

    let mut component = instantiate_typed_with_ctx(&wat, ctx_with_parent(sender, parent), type_tag);

    assert_eq!(component.read_u32(204), 0x4c22);
    assert_eq!(component.read_u32(208), 0x4c23);
}

#[test]
fn legacy_typed_init_still_loads_when_the_binding_has_a_parent() {
    let sender = MailboxId(0x4c31);
    let parent = MailboxId(0x4c32);
    let type_tag = 0x4c33;
    let wat = wat_typed_init("init_typed_p32", false);

    let mut component = instantiate_typed_with_ctx(&wat, ctx_with_parent(sender, parent), type_tag);

    assert_eq!(component.read_u32(208), 0x4c33);
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
    assert_eq!(component.read_bytes(small_ptr as usize, payload.len()), payload,);
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
    assert_ne!(large_ptr, component.small_ptr, "large config must not land in the small region");
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
    assert!(msg.contains("no realloc_p32 allocator"), "error should name the missing allocator; got: {msg}");
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
    assert_eq!(component.read_u32(100), 0x77, "wire must run when Component::wire is invoked");
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
    assert!(result.is_err(), "Component::wire must propagate the guest trap as wasmtime::Error");
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
    assert_eq!(component.read_u32(16), small_ptr, "small payload should land in the cached small region");
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
    assert_eq!(component.read_u32(16), large_ptr, "large payload should land in the grown large region");
}

/// ADR-0095: a payload past the absolute deliverable ceiling is dropped
/// cleanly (no trap, no write) even when the guest exports an allocator —
/// the guard fires on the length check.
#[test]
fn deliver_oversize_payload_dropped() {
    let mut component = instantiate(&wat_records_mail_ptr());
    let mail = Mail::new(MailboxId(0), aether_data::KindId(0), vec![0u8; MAX_DELIVERABLE_MAIL_BYTES + 1], 1);
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
    let bundle = StateBundle { version: 0x2A, bytes: vec![0x01, 0x02, 0x03, 0x04, 0x05] };
    component.call_on_rehydrate(&bundle).expect("rehydrate ok");
    // Hook copied the version to offset 396 and the bytes to 400.
    assert_eq!(component.read_u32(396), 0x2A);
    assert_eq!(component.read_bytes(400, 5), vec![0x01, 0x02, 0x03, 0x04, 0x05],);
}

#[test]
fn call_on_rehydrate_without_export_is_noop() {
    let mut component = instantiate(WAT_NO_HOOKS);
    let bundle = StateBundle { version: 1, bytes: vec![9, 9, 9] };
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
    assert_eq!(component.store.data().reply_table.resolve(observed), Some(ReplyEntry::session(token)),);
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
    assert_eq!(component.store.data().reply_table.resolve(observed), Some(ReplyEntry::component(M(7))),);
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

/// Issue 2791: fixture whose `receive_p32` reads the additive
/// `reply_correlation_p32` import and stores the low 32 bits at offset 500.
fn wat_stores_reply_correlation() -> String {
    format!(
        r#"
        (module
            (import "aether" "reply_correlation_p32"
                (func $reply_correlation (result i64)))
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 500
                call $reply_correlation
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
    assert_eq!(component.read_u32(500), 0, "a session origin must thread 0 (MailboxId::NONE) as the source param");
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
    assert_eq!(component.read_u32(500), 0, "a no-reply-target origin must thread 0 as the source param");
}

#[test]
fn reply_correlation_import_exposes_reply_envelope_only() {
    use crate::mail::{Mail as SubstrateMail, MailboxId as M, Source, SourceAddr};

    let mut component = instantiate(&wat_stores_reply_correlation());

    let reply = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
        .with_reply_to(Source::with_correlation(SourceAddr::None, 0x5151));
    component.deliver(&reply).expect("deliver reply");
    assert_eq!(component.read_u32(500), 0x5151, "reply envelope must expose its echoed correlation");

    let request = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
        .with_reply_to(Source::with_correlation(SourceAddr::Component(M(7)), 0x9999));
    component.deliver(&request).expect("deliver request");
    assert_eq!(
        component.read_u32(500),
        0,
        "request envelope correlation is in the sender's id space and must not surface",
    );
}

fn plane_ctx_for_reply() -> (ComponentCtx, Receiver<EgressEvent>, aether_data::KindId) {
    use crate::mail::MailboxId as M;
    use aether_data::{KindDescriptor, SchemaType};

    let (outbound, rx) = HubOutbound::attached_loopback();
    let registry = Arc::new(Registry::new());
    let pong_id = registry
        .register_kind_with_descriptor(
            &boot_authority(),
            KindDescriptor { name: "test.pong".into(), schema: SchemaType::Unit },
        )
        .expect("register kind");
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
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

fn wat_scoped_spawns(parent: MailboxId) -> String {
    format!(
        r#"
        (module
            (import "aether" "spawn_sibling_scoped_p32"
                (func $spawn_sibling (param i64 i64 i32 i32 i32 i32 i32) (result i64)))
            (import "aether" "spawn_inline_child_scoped_p32"
                (func $spawn_inline (param i64 i32 i32 i32) (result i64)))
            (memory (export "memory") 1)
            (data (i32.const 32) "leaf")
            (data (i32.const 48) "worker")
            (data (i32.const 64) "cfg")
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 200
                i64.const {parent}
                i32.const 0
                i32.const 32
                i32.const 4
                call $spawn_inline
                i64.store
                i32.const 208
                i64.const {parent}
                i64.const 4660
                i32.const 0
                i32.const 48
                i32.const 6
                i32.const 64
                i32.const 3
                call $spawn_sibling
                i64.store
                i32.const 0))
        "#,
        parent = parent.0,
    )
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
    let EgressEvent::ToSession { session, kind_name, .. } = event else {
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
            &boot_authority(),
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
        .register_kind_with_descriptor(
            &boot_authority(),
            KindDescriptor { name: "test.pong".into(), schema: SchemaType::Unit },
        )
        .expect("register kind");

    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let ctx = ComponentCtx::new(M(0), Arc::clone(&registry), mailer, HubOutbound::disconnected());
    let mut component = instantiate_with_ctx(&wat_replies(pong_id.0), ctx);

    // Inbound whose reply target is a peer component, carrying
    // `INBOUND_CORRELATION`. The guest's `receive_p32` calls
    // `reply_mail_p32` with the sender handle the substrate
    // allocated for this reply target.
    let mail = SubstrateMail::new(M(0), aether_data::KindId(0), vec![], 1)
        .with_reply_to(Source::with_correlation(SourceAddr::Component(recipient), INBOUND_CORRELATION));
    component.deliver(&mail).expect("deliver");

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1, "reply should have dispatched once");
    let reply_to = captured[0];
    assert_eq!(
        reply_to.correlation_id, INBOUND_CORRELATION,
        "reply must echo the inbound correlation, not a fresh mint",
    );
    assert_eq!(reply_to.addr, SourceAddr::None, "reply-of-a-reply target must be None, matching native send_reply");
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
        .try_register_inbox(&boot_authority(), "client", registry::noop_handler())
        .expect("register client mailbox");

    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(Arc::clone(&outbound)));

    let ctx = ComponentCtx::new(sender, Arc::clone(&registry), Arc::clone(&mailer), outbound);

    let unknown = MailboxId(0xDEAD_BEEF_u64);
    let kind = aether_data::KindId(0xABCD_u64);
    // `from = NONE` → the dispatch identity falls back to `self.sender`.
    ctx.send(unknown, kind, vec![1, 2, 3], 1, MailboxId::NONE);

    let event = outbound_rx.try_recv().expect("bubble-up event emitted");
    match event {
        EgressEvent::UnresolvedMail { recipient_mailbox_id, kind_id, payload, count, source_mailbox_id, .. } => {
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
        .try_register_inbox(&boot_authority(), "client", registry::noop_handler())
        .expect("register client mailbox");

    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    // Deliberately no `with_outbound` — exercises the local warn-drop path.

    let ctx = ComponentCtx::new(sender, Arc::clone(&registry), Arc::clone(&mailer), outbound);

    ctx.send(MailboxId(0xDEAD_BEEF_u64), aether_data::KindId(0xABCD), vec![], 0, MailboxId::NONE);
    assert!(outbound_rx.try_recv().is_err(), "no bubble-up without a wired outbound");
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

    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x42));
    let ctx = ComponentCtx::new(sender, Arc::clone(&registry), Arc::clone(&mailer), HubOutbound::disconnected());

    // Inbound lineage: the chassis-driven tick chain we're "in"
    // when the wasm guest's on_tick handler fires its outbound.
    let inbound_root = MailId::new(MailboxId::CHASSIS_MAILBOX_ID, 7);
    let inbound_mail = MailId::new(MailboxId(aether_data::with_tag(Tag::Mailbox, 0x99)), 42);
    ctx.set_in_flight(inbound_mail, inbound_root);

    ctx.send(sink_id, aether_data::KindId(0xABCD), vec![1, 2, 3], 1, MailboxId::NONE);

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1, "sink should have been called once");
    let (mail_id, root, parent) = captured[0];
    assert_eq!(parent, Some(inbound_mail), "parent_mail must point at inbound");
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
    let (captured, sink_id) = register_lineage_capture_sink(&registry, "issue_722_fresh_root_sink");

    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x33));
    let ctx = ComponentCtx::new(sender, Arc::clone(&registry), Arc::clone(&mailer), HubOutbound::disconnected());
    // No `set_in_flight` call.

    ctx.send(sink_id, aether_data::KindId(0xCAFE), vec![], 1, MailboxId::NONE);

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
    let (captured, sink_id) = register_lineage_capture_sink(&registry, "issue_1802_detached_sink");

    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x55));
    let ctx = ComponentCtx::new(sender, Arc::clone(&registry), Arc::clone(&mailer), HubOutbound::disconnected());

    // Set an in-flight chain the default `send` would inherit.
    let inbound_root = MailId::new(MailboxId::CHASSIS_MAILBOX_ID, 9);
    let inbound_mail = MailId::new(MailboxId(aether_data::with_tag(Tag::Mailbox, 0x77)), 13);
    ctx.set_in_flight(inbound_mail, inbound_root);

    ctx.send_detached(sink_id, aether_data::KindId(0xF00D), vec![7, 8], 1, MailboxId::NONE);

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1, "sink should have been called once");
    let (mail_id, root, parent) = captured[0];
    assert!(parent.is_none(), "detached send carries no parent edge despite in-flight");
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
        aether_data::fold_lineage(parent_carry, aether_data::ActorId::instanced(TRAMPOLINE_NAMESPACE, "widget")),
    ));
    let from_path =
        aether_data::mailbox_id_from_path("aether.component/aether.embedded:testparent/aether.embedded:widget");
    assert_eq!(folded, from_path, "the host-fn alias fold matches the rendered-name parse → fold");
}

/// Issue 4490: both scoped spawn imports accept a freshly prepared inline
/// actor as the executing parent, extend that actor's lineage, and preserve
/// the validated identity through the detached-spawn staging seam. Keeping
/// the parent alias owner-unpublished exercises the immediate nested `wire`
/// window as well as the ordinary handler path.
#[test]
fn scoped_wasm_spawns_extend_the_executing_inline_actor() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let root_name = "aether.component/aether.embedded:nested-root";
    let root = aether_data::mailbox_id_from_path(root_name);
    let (_captured, root_handler) = lineage_capture_handler();
    registry
        .try_register_inbox_with_id(&boot_authority(), root, root_name, root_handler)
        .expect("register component root");

    let parent_name = format!("{root_name}/{TRAMPOLINE_NAMESPACE}:branch");
    let parent = aether_data::mailbox_id_from_path(&parent_name);
    let mut ctx = ComponentCtx::new(root, Arc::clone(&registry), mailer, HubOutbound::disconnected());
    ctx.stage_alias(PreparedAliasRoute::new(parent, parent_name.clone(), root));
    let mut component = instantiate_with_ctx(&wat_scoped_spawns(parent), ctx);

    component.deliver(&Mail::new(parent, aether_data::KindId(0), Vec::new(), 1)).expect("deliver nested spawn turn");

    let expected_inline_name = format!("{parent_name}/{TRAMPOLINE_NAMESPACE}:leaf");
    let expected_inline = aether_data::mailbox_id_from_path(&expected_inline_name);
    let aliases = component.drain_pending_aliases();
    let inline = aliases.iter().find(|alias| alias.alias == expected_inline).expect("nested inline alias staged");
    assert_eq!(&*inline.rendered_name, expected_inline_name);
    assert_eq!(inline.target_parent, root, "nested aliases still route to the physical trampoline root");

    let expected_detached_name = format!("{parent_name}/{TRAMPOLINE_NAMESPACE}:worker");
    let expected_detached = aether_data::mailbox_id_from_path(&expected_detached_name);
    let spawns = component.drain_pending_spawns();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].parent, parent);
    assert_eq!(spawns[0].parent_name, parent_name);
    assert_eq!(spawns[0].subname, "worker");
    assert_eq!(spawns[0].config, b"cfg");
    let returned_inline = u64::from(component.read_u32(200)) | (u64::from(component.read_u32(204)) << 32);
    let returned_detached = u64::from(component.read_u32(208)) | (u64::from(component.read_u32(212)) << 32);
    assert_eq!(returned_inline, expected_inline.0, "guest receives the nested inline id");
    assert_eq!(returned_detached, expected_detached.0, "guest receives the nested detached id");
}

/// The new scalar is guest-controlled input, not authority. A foreign
/// mailbox must allocate neither an alias nor a detached birth and both
/// imports return the zero sentinel.
#[test]
fn scoped_wasm_spawns_reject_a_foreign_parent() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let root_name = "aether.component/aether.embedded:scoped-root";
    let root = aether_data::mailbox_id_from_path(root_name);
    let (_captured, root_handler) = lineage_capture_handler();
    registry
        .try_register_inbox_with_id(&boot_authority(), root, root_name, root_handler)
        .expect("register component root");
    let foreign = aether_data::mailbox_id_from_path("aether.component/aether.embedded:foreign");
    let ctx = ComponentCtx::new(root, registry, mailer, HubOutbound::disconnected());
    let mut component = instantiate_with_ctx(&wat_scoped_spawns(foreign), ctx);

    component.deliver(&Mail::new(root, aether_data::KindId(0), Vec::new(), 1)).expect("deliver rejected spawn turn");

    assert!(component.drain_pending_aliases().is_empty());
    assert!(component.drain_pending_spawns().is_empty());
    assert_eq!(component.read_u32(200), 0);
    assert_eq!(component.read_u32(204), 0);
    assert_eq!(component.read_u32(208), 0);
    assert_eq!(component.read_u32(212), 0);
}

/// ADR-0114 + ADR-0165: a logical alias route follows the parent's `Inbox`
/// without retaining its handler, and the rendered alias name resolves (the
/// engine's `Call` recipient-name path) to the alias id.
#[test]
fn inline_alias_routes_into_parent_slot_inbox() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let (captured, capture_handler) = lineage_capture_handler();
    let parent_name = "aether.component/aether.embedded:testparent".to_owned();
    let parent_id = aether_data::mailbox_id_from_path(&parent_name);
    registry
        .try_register_inbox_with_id(&boot_authority(), parent_id, parent_name.clone(), capture_handler)
        .expect("parent registers under its lineage id");
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );

    // Mirror the host/trampoline split: fold the alias id, then let the owner
    // publish only the logical alias-to-parent relation.
    let alias_name = format!("{parent_name}/aether.embedded:widget");
    let alias_id = aether_data::mailbox_id_from_path(&alias_name);
    let completion = registry
        .submit(EffectBatch::new(vec![RegistryEffect::PublishAlias(PreparedAliasRoute::new(
            alias_id,
            alias_name.clone(),
            parent_id,
        ))]))
        .expect("owner accepts the alias batch");
    owner.run_once();
    completion
        .wait_timeout(Duration::from_millis(100))
        .expect("alias completion arrives")
        .expect("alias route publishes");

    // Name resolution (the wire `Call` path) resolves the alias.
    assert_eq!(registry.lookup(&alias_name), Some(alias_id), "the rendered alias name resolves to the folded alias id");

    // Mail addressed to the alias lands in the parent slot's inbox.
    mailer.push(Mail::new(alias_id, aether_data::KindId(0xABCD), vec![1, 2, 3], 1));
    assert_eq!(captured.lock().unwrap().len(), 1, "alias mail dispatched into the parent slot's inbox");
}

/// Issue 1987: a guest `send` carrying `from == self` (a
/// normally-addressed actor) stamps the component as origin — the no-op
/// regression that guards a normally-addressed actor.
#[test]
fn send_stamps_self_when_recipient_is_own_mailbox() {
    let registry = Arc::new(Registry::new());
    let (captured, sink_id) = register_lineage_capture_sink(&registry, "inline_self_origin_sink");
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x42));
    let ctx = ComponentCtx::new(sender, Arc::clone(&registry), Arc::clone(&mailer), HubOutbound::disconnected());

    // `from == self` (a normally-addressed actor).
    ctx.send(sink_id, aether_data::KindId(0xABCD), vec![], 1, sender);

    let captured = captured.lock().unwrap();
    let (mail_id, _root, _parent) = captured[0];
    assert_eq!(mail_id.sender, sender, "origin stamps the component's own id when from == self");
}

/// Issue 1987: a guest `send` carrying `from == an inline-child alias`
/// stamps the alias as origin — the guest-carried `from` becomes the
/// dispatch identity, so the child's sends carry the child's address.
#[test]
fn send_stamps_alias_when_recipient_is_inline_child() {
    let registry = Arc::new(Registry::new());
    let (captured, sink_id) = register_lineage_capture_sink(&registry, "inline_alias_origin_sink");
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x42));
    let alias = MailboxId(aether_data::with_tag(Tag::Mailbox, 0xA11A5));
    let ctx = ComponentCtx::new(sender, Arc::clone(&registry), Arc::clone(&mailer), HubOutbound::disconnected());

    // `from == an inline-child alias` distinct from the component's own id.
    ctx.send(sink_id, aether_data::KindId(0xABCD), vec![], 1, alias);

    let captured = captured.lock().unwrap();
    let (mail_id, _root, _parent) = captured[0];
    assert_eq!(mail_id.sender, alias, "origin stamps the alias (dispatch identity) when from is a child");
    assert_ne!(mail_id.sender, sender, "the child's send must not stamp the parent component");
}

/// ADR-0165: a freshly created inline child runs `init` and `wire` before its
/// logical route reaches the owner. Its locally prepared alias must already be
/// trusted as an in-cluster identity, or sends from those hooks are
/// incorrectly attributed to the physical parent.
#[test]
fn pending_inline_alias_is_trusted_before_owner_publication() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let sender = MailboxId(aether_data::with_tag(Tag::Mailbox, 0x42));
    let alias = MailboxId(aether_data::with_tag(Tag::Mailbox, 0xA11A5));
    let mut ctx = ComponentCtx::new(sender, Arc::clone(&registry), mailer, HubOutbound::disconnected());
    ctx.stage_alias(PreparedAliasRoute::new(alias, "pending-inline-alias", sender));

    assert!(!registry.is_alias_to(alias, sender), "the owner has not published the route yet");
    assert!(host_fns::is_own_cluster_alias(&ctx, alias), "the prepared alias is already trusted in-cluster");
    assert!(
        !host_fns::is_own_cluster_alias(&ctx, MailboxId(aether_data::with_tag(Tag::Mailbox, 0xF0E1))),
        "a local prepared fact does not admit an unrelated identity"
    );
}
