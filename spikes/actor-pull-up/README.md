# Spike: `#[actor]` on the capability, behavior in the runtime module

## Question

The ADR-0122 identity/runtime split puts the `#[actor] impl` (dispatcher +
handler bodies) in the cap's identity file (`mod.rs`) and only the runtime
*state* in `runtime.rs`. We want the inverse: behavior next to the state it
drives (in `runtime.rs`), the `#[actor]` macro on the **capability struct**,
and the always-on addressing markers reachable from the identity even when the
runtime module is `#[cfg]`-stripped.

A proc-macro emits at one site, so it can't put markers in `mod.rs` and a
dispatcher in `runtime.rs` from one invocation — *unless* it reads the runtime
file off disk to harvest what it needs. This spike proves that works on the
pinned stable toolchain (Rust 1.96).

## Target shape (proven)

```rust
// mod.rs — identity
#[actor(singleton)]          // module name defaults to the sibling `runtime`
pub struct RenderCapability; // (`#[actor(singleton, other)]` overrides it)

#[cfg(feature = "render-native")]
mod runtime;
```
```rust
// runtime.rs — behavior, gated with the module
#[runtime]
impl Runtime for RenderCapability {
    const NAMESPACE: &str = "spike.render";   // consumed by #[actor]
    type State = RenderCapabilityState;
    fn init() -> RenderCapabilityState { .. } // lifecycle — kept
    #[handler] fn on_tick(..) { .. }           // dispatch    — kept
}
pub struct RenderCapabilityState { .. }
```

- `#[actor(singleton, runtime)]` on the struct reads `runtime.rs`, pulls the
  `NAMESPACE` const and the `#[handler]` kinds out of the impl, and emits the
  always-on identity: `impl Addressable` (namespace + `Resolver = One` from
  `singleton`) and one `impl Handles<K>` per kind. Nothing is restated on the
  struct — the namespace and kinds are read, not re-declared.
- `#[runtime]` on the impl emits the behavior — the `Runtime` (lifecycle +
  state) impl plus the handler bodies as an inherent impl — and *consumes* the
  `NAMESPACE` const (it belongs to `Addressable`, not the behavior trait). It
  rides the module's `#[cfg]`.

(`Handles<K>` ≈ `HandlesKind<K>`, `Addressable` ≈ the real one, `Runtime` ≈ the
gated `Lifecycle`/`Dispatch`/`NativeActor` surface. The real `#[actor]` would
also emit the always-on name-inventory entry; omitted here.)

## Why it's an attribute on the struct, not on `mod runtime;`

The natural-looking `#[actor] mod runtime;` is **rejected on stable** —
E0658, [rust#54727]: *file modules in proc-macro input are unstable* (inline
`mod m { .. }` is stable; the `mod m;` file form is not, and never reaches the
macro). A struct is ordinary proc-macro input, so hosting `#[actor]` on the cap
struct sidesteps it entirely; `mod runtime;` stays a plain, author-written,
`#[cfg]`-gated line that no macro touches.

## How the read works

`proc_macro::Span::local_file()` ([rust#140514], stable since 1.88) resolves the
on-disk path of the file holding the `#[actor]` invocation. The sibling runtime
file (`<name>.rs` else `<name>/mod.rs`) is read with `fs::read_to_string` and
parsed with `syn` — cfg-agnostically, so the identity is harvested even in a
configuration where `mod runtime` is stripped.

## Evidence

```
cargo build -p consumer --no-default-features   # mod runtime STRIPPED → exit 0
cargo build -p consumer --features runtime       # full behavior        → exit 0
cargo test  -p consumer --no-default-features    # 1 passed
cargo test  -p consumer --features runtime       # 2 passed
cargo clippy --workspace --all-targets           # clean
```

- **Feature-off** is the proof: `mod runtime` is stripped, so the `Addressable`
  + `Handles<_>` impls can't come from compiling `runtime.rs`. Yet
  `_assert_identity_present` (forcing `RenderCapability: Handles<Tick> +
  Handles<Resize> + Addressable<Resolver = One>`) compiles, and the
  `namespace_lifted_from_runtime_const` test passes
  (`NAMESPACE == "spike.render"`). All of it came from `#[actor]`'s disk read.
- **Lifecycle survives the split:** the feature-on `lifecycle_init_runs` test
  calls `<RenderCapability as Runtime>::init()` — emitted by `#[runtime]` from
  the `fn init` in the impl. The split loses no behavior; only the markers float
  up.
- An earlier negative control (requiring a marker for a non-`#[handler]` kind →
  E0277) confirmed the lift is precise, not a blanket impl.

## Caveats / open questions

- **rust-analyzer is the unverified risk, and it's the one that matters.** A
  disk-reading macro is exactly where RA's in-memory VFS can diverge from
  `cargo` — it may run `#[actor]` against a stale buffer, a different path, or
  skip the read. If RA fails to resolve the lifted identity, typed
  `ctx.actor::<Cap>().send(&kind)` goes red in the editor, which undercuts the
  whole reason the markers are always-on. **Verify in RA/RustRover before
  adopting.** `cargo` correctness (proven here) is necessary, not sufficient.
- **`local_file()` is `Option`** — `None` under `--remap-path-prefix` (some
  sandboxed / distributed builds). `#[actor]` hard-errors there; a real version
  needs a defined fallback.
- **Lifted kind tokens are verbatim**, so handler arg types must resolve in the
  *identity* module's scope, not just the runtime module's — write them bare
  (`Tick`, not `super::Tick`) or the lifted `impl` won't resolve. A real macro
  needs a path-normalization story.
- **Harvest ignores cfg** (syn doesn't evaluate it), so cfg'd-out handlers are
  also lifted.
- **Incremental is fine.** `runtime.rs` is a real crate module, so editing it
  changes the crate fingerprint → recompile → `#[actor]` re-runs and re-reads.
  The unstable `tracked_path` API ([rust#99515]) is only needed for non-module
  asset files, which this is not.

## Comparison

This competes with the no-macro "Option A": leave `mod runtime;` ungated and
gate only the substrate-typed items inside it, letting the existing
`#[actor(runtime_feature = ...)]` emit markers ungated from within `runtime.rs`
(crate-global impls reach the identity for free). Option A needs no new
machinery and is RA-clean; this spike's form gives the cleaner authoring model
(`#[actor]` on the cap, behavior + state together in the runtime file) at the
cost of a disk-reading macro and the RA risk above.

[rust#54727]: https://github.com/rust-lang/rust/issues/54727
[rust#140514]: https://github.com/rust-lang/rust/pull/140514
[rust#99515]: https://github.com/rust-lang/rust/issues/99515
