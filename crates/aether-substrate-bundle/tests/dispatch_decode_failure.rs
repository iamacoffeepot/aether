//! iamacoffeepot/aether#2455 regression: the wasm `#[actor]` dispatch table must
//! report a recognized kind id with an undecodable payload as
//! `DISPATCH_UNKNOWN_KIND` (falling through to the strict-receiver tail), not
//! `DISPATCH_HANDLED`. Pre-fix the macro returned `DISPATCH_HANDLED`
//! unconditionally once the kind id matched, so a corrupt/truncated payload for a
//! known kind reported success while the handler never ran and no reply was
//! emitted — diverging from the native arm (which routes the same case to the
//! fallback / unknown-kind path) and hanging a request-shaped caller to its
//! settlement timeout with no diagnostic.
//!
//! Drives the strict `Probe` fixture (namespace `test_fixture_probe`, no
//! `#[fallback]`, an `on_set_render(SetRender)` handler) directly through the pub
//! `Component::instantiate` / `deliver` API: deliver a `Mail` carrying
//! `SetRender::ID` (a `#[repr(C)]` 4-byte cast-shape kind) with a 2-byte payload,
//! which the cast decoder rejects on its `len() == size_of` check, and assert the
//! dispatch return code. Gated on `require_runtime` like the sibling integration
//! tests, so it skips cleanly on a driverless / wasm-not-built box.

use std::fs;
use std::sync::Arc;

use aether_data::Kind;
use aether_substrate::actor::wasm::host_fns;
use aether_substrate::{Component, ComponentCtx, HubOutbound, Mail, MailboxId, Mailer, Registry};
use aether_substrate_bundle::test_bench::test_helpers::require_runtime;
use aether_test_fixtures_kinds::SetRender;
use wasmtime::{Engine, Linker, Module};

#[test]
fn known_kind_bad_payload_reports_unknown_kind_not_handled() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let engine = Engine::default();
    let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
    host_fns::register(&mut linker).expect("register host fns");
    let module = Module::new(&engine, &wasm).expect("compile fixture module");

    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let ctx = ComponentCtx::new(MailboxId(0), registry, mailer, HubOutbound::disconnected());

    // `type_tag = None` instantiates the module's entry actor — `Probe`, the
    // strict (no-`#[fallback]`) receiver (export!(Probe, ProbeWithConfig) makes
    // the first-listed `Probe` the entry).
    let mut component = Component::instantiate(&engine, &linker, &module, ctx, &[], None)
        .expect("instantiate Probe fixture");

    // `SetRender` is a `#[repr(C)]` 4-byte cast-shape kind; a 2-byte payload
    // fails `decode_cast`'s `len() == size_of` check, so the matched dispatch
    // arm's `decode_kind::<SetRender>()` is `None`.
    let mail = Mail::new(MailboxId(0), SetRender::ID, vec![0u8, 0u8], 1);
    let rc = component.deliver(&mail).expect("deliver");

    // Tripwire: pre-fix the arm returned `DISPATCH_HANDLED` (0) once the kind id
    // matched, regardless of decode outcome; post-fix the failed decode falls
    // through to the strict tail → `DISPATCH_UNKNOWN_KIND` (1).
    assert_eq!(
        rc,
        aether_actor::DISPATCH_UNKNOWN_KIND,
        "a recognized kind with an undecodable payload must fall through to the \
         tail (DISPATCH_UNKNOWN_KIND), not report DISPATCH_HANDLED",
    );
    assert_ne!(rc, aether_actor::DISPATCH_HANDLED);
}
