//! Pass `#[actor]` expansions: lineage, addressing, handler classes (shard B of #5133).
//!
//! Suite layout and `.stderr` regeneration: `tests/ui/shard_support.rs`.

#[path = "ui/shard_support.rs"]
mod shard_support;

#[test]
fn ui_b() {
    let t = shard_support::cases("ui_b");
    t.pass("tests/ui/accepts_minimal_actor.rs");
    t.pass("tests/ui/accepts_generic_local.rs");
    t.pass("tests/ui/accepts_actor_lineage_wasm.rs");
    t.pass("tests/ui/accepts_actor_composable_wasm.rs");
    // ADR-0119 parent-scope amendment: a wasm actor's macro-emitted
    // `Embedded` resolver is admitted on ordinary typed ctx and MailSender
    // surfaces, which select the caller's runtime parent mailbox.
    t.pass("tests/ui/accepts_bare_type_address_of_embedded_peer.rs");
    // ADR-0112: the manual reply class compiles. The native manual-class
    // behavior is covered by the `manual_handler_replies_through_ctx`
    // integration test in `aether-substrate` (this proc-macro crate has no
    // `aether-substrate` dev-dep, so a native *pass* / type-error fixture
    // can't link the substrate types — the existing native fixtures here
    // are all macro-level diagnostics that fire before path resolution).
    t.pass("tests/ui/accepts_manual_handler_wasm.rs");
    // ADR-0169: the handler-set delegation seam — the set's dispatch method
    // must be callable from the adopter's table and its manifest const usable
    // in the adopter's const-array arithmetic.
    t.pass("tests/ui/accepts_handler_set_wasm.rs");
    // ADR-0134: the multi reply class compiles on both expansion paths. The
    // wasm fixture type-checks the full `emit` body; the native fixture uses
    // the split + runtime-feature gate (like `accepts_actor_split_task_handler`)
    // so the substrate-typed runtime surface cfgs out, leaving the assertion
    // that the macro accepts the `#[handler::multi]` + `Multi<K>` signature.
    t.pass("tests/ui/accepts_multi_handler_wasm.rs");
    t.pass("tests/ui/accepts_multi_handler_native.rs");
}
