//! Pass `#[actor]` expansions: state, split, struct-hosted, cfg gates (shard C of #5133).
//!
//! Suite layout and `.stderr` regeneration: `tests/ui/shard_support.rs`.

#[path = "ui/shard_support.rs"]
mod shard_support;

#[test]
fn ui_c() {
    let t = shard_support::cases("ui_c");
    // ADR-0113: declarative `type State` + `dehydrate` / `rehydrate`
    // accessors generate the hot-swap hooks.
    t.pass("tests/ui/accepts_state_actor.rs");
    // iamacoffeepot/aether#2330: the `#[actor]` split path gains a
    // `runtime_feature = "name"` gate override. The substrate-typed runtime
    // impls cfg out in the fixture bin (no `runtime`/named feature), so the pass
    // fixture exercises the marker + name-inventory surface the arg drives.
    t.pass("tests/ui/accepts_actor_runtime_feature.rs");
    // iamacoffeepot/aether#2338: a split `#[actor]` may carry a `#[fallback]`
    // whose first param is `state: &mut Self::State` (the validator gained the
    // `is_split` branch the split `#[handler]` path already had).
    t.pass("tests/ui/accepts_actor_split_fallback.rs");
    // iamacoffeepot/aether#2341: a split `#[actor]` may carry a `#[handler(task)]`
    // whose first param is `state: &mut Self::State` (the last native-split
    // first-param validator to gain the `is_split` branch).
    t.pass("tests/ui/accepts_actor_split_task_handler.rs");
    // ADR-0123 struct-hosted `#[actor]` happy path: the disk-read harvest selects
    // the runtime module's `impl NativeActor` (gap-1 trait filter), lifts its
    // identity, and emits the addressing markers plus the gap-3 `include_bytes!`
    // rebuild edge. `rt_ok.rs` is the sibling stub read off disk, never compiled.
    t.pass("tests/ui/accepts_struct_hosted_actor.rs");
    // The module-path form of the same harvest: `#[actor(singleton,
    // nested::rt_nested)]` joins the path segments into a file path relative
    // to the invoking file (`nested/rt_nested.rs`) — the headless-companion
    // layout (`runtime::headless`). `nested/rt_nested.rs` is read off disk,
    // never compiled.
    t.pass("tests/ui/accepts_struct_nested_runtime.rs");
    // iamacoffeepot/aether#4811: a `#[cfg]` on a handler governs every artifact
    // the expansion derives from it. Each fixture compiles for the host with one
    // handler gated in and one gated out, so a leaked dispatch arm, marker impl,
    // manifest record, or retention static names a type and a method that do not
    // exist in that configuration and fails the build.
    t.pass("tests/ui/accepts_cfg_gated_handler_wasm.rs");
    t.pass("tests/ui/accepts_cfg_gated_handler_native.rs");
    // ADR-0183: the same contract on the sibling macro. A wasm set emits arms
    // and manifest records and no marker bridge, so this fixture pins the replay
    // half: the stripped handler's kind type is gated with it, so a leaked
    // artifact names a type that is not there, and the surviving records are
    // compared byte-for-byte against an ungated set declaring exactly the
    // handlers that outlive `#[cfg(test)]`, so over-stripping fails too. The
    // bridge is a native-set artifact, and it is asserted twice elsewhere: its
    // gate pair over the emitted tokens in `handler_set::tests`, and its effect
    // on a real adopter in `aether-substrate/tests/native_actor_macro.rs`.
    // Neither belongs here, because a native set's expansion names
    // `aether_substrate` types and `aether-substrate` depends transitively on
    // this crate. A dev-dependency can close that cycle, so what rules a fixture
    // out here is cost, not the dependency graph: it would pull the wasmtime and
    // cranelift tree into these UI tests, which the substrate side of the edge
    // builds anyway.
    t.pass("tests/ui/accepts_cfg_gated_handler_set_wasm.rs");
}
