# Spike: pulling runtime handler-markers up to the identity module

## Question

The ADR-0122 identity/runtime split puts the `#[actor] impl` (dispatcher +
handler bodies) in the cap's identity file (`mod.rs`) and only the runtime
*state* in `runtime.rs`. We want the inverse: the dispatcher next to the state
it drives (in `runtime.rs`), with the always-on addressing markers
(`HandlesKind<K>`) still reachable from the identity even when the runtime
module is `#[cfg]`-stripped.

A proc-macro emits at one site, so it cannot put markers in `mod.rs` and a
dispatcher in `runtime.rs` from one invocation **unless** it can read the
runtime file off disk to harvest the handler kinds. This spike tests whether
that disk-read approach is viable on the pinned stable toolchain (Rust 1.96).

## Result

| Form | Verdict |
| --- | --- |
| `#[pull_up] mod runtime;` (attribute on a file module) | **Blocked on stable** — E0658, [rust#54727]: *file modules in proc-macro input are unstable*. Inline `mod m { .. }` is stable; the `mod m;` file form is not, and never reaches the macro. |
| `lift_markers!(runtime => Identity)` (function-like macro) + a plain hand-gated `mod runtime;` | **Works on stable.** The file module is in neither the macro's input nor its output, so #54727 never triggers. |

The capability the user asked about — auto-lifting markers with the kinds
harvested from the runtime file — is achievable. The exact *spelling*
(`#[pull_up] mod runtime;`) is not; it costs one extra line and a slightly
different shape.

## What the macro does (working form)

`pull-up-macro/src/lib.rs` exposes `lift_markers!(module => Identity)`:

1. `proc_macro::Span::local_file()` (stable since 1.88, [rust#140514]) resolves
   the on-disk path of the file holding the invocation.
2. Resolve the sibling module file (`<dir>/<name>.rs` else `<dir>/<name>/mod.rs`).
3. `fs::read_to_string` + `syn::parse_file` — cfg-agnostic, so the kinds are
   harvested even in a config where the module is stripped.
4. Collect the type of each `#[handler]` method's last typed argument.
5. Emit `impl Handles<K> for Identity {}` per kind, at the invocation site.

`mod runtime;` stays a plain, author-written, `#[cfg(feature = "runtime")]`
line. `Handles<K>` stands in for `aether_actor::HandlesKind<K>`.

## Evidence

```
cargo build -p consumer --no-default-features   # mod runtime STRIPPED → exit 0
cargo build -p consumer --features runtime       # full dispatcher       → exit 0
```

The feature-off build is the proof: `mod runtime` is stripped, so the marker
impls cannot come from compiling `runtime.rs` — yet `_assert_markers_present`
(which forces `RenderCapability: Handles<Tick> + Handles<Resize>` via a
turbofish call site) compiles. The impls came only from the macro's disk read.

Negative control (temporary, reverted): requiring `Handles<Unhandled>` for a
kind with no `#[handler]` fails with `E0277` — the lift is precise (per-kind),
not a blanket `impl<K> Handles<K>`.

## Caveats / open questions

- **rust-analyzer is the unverified risk, and it's the one that matters.** A
  disk-reading macro is exactly where RA's in-memory VFS can diverge from what
  `cargo build` sees — RA may run the macro against a stale buffer, a different
  path, or skip the read. If RA fails to resolve the lifted markers, typed
  `ctx.actor::<Cap>().send(&kind)` goes red in the editor, which undercuts the
  whole reason the markers are always-on. **Verify in RA/RustRover before
  adopting.** `cargo` correctness (proven here) is necessary, not sufficient.
- **`local_file()` is `Option`** — `None` under `--remap-path-prefix` (some
  sandboxed / distributed builds). The macro hard-errors there; a real version
  needs a defined fallback.
- **Lifted type tokens are verbatim.** Handler arg types and the impl self-type
  must resolve in the *identity* module's scope, not just the runtime module's
  — so they must be written bare (`Tick`, not `super::Tick`) or the lifted
  `impl` won't resolve. A real macro would need a path-normalization story.
- **Harvest ignores cfg** (syn doesn't evaluate it), so cfg'd-out handlers are
  also lifted. Controllable, but a sharp edge.
- **Incremental is fine here.** `runtime.rs` is a real crate module, so editing
  it changes the crate fingerprint → recompile → macro re-runs and re-reads.
  The unstable `tracked_path` API ([rust#99515]) is only needed for non-module
  asset files, which this is not.
- **Ergonomics:** two lines (`#[cfg] mod runtime;` + `lift_markers!(...)`) and a
  re-stated identity name in the macro call, versus the dreamed one-liner.

## Comparison

This competes with the no-macro alternative ("Option A"): leave `mod runtime;`
ungated and gate only the substrate-typed items inside it, letting the existing
`#[actor(runtime_feature = ...)]` emit markers ungated from within `runtime.rs`
(crate-global impls reach the identity for free). Option A needs no new
machinery and is RA-clean; this spike's form gives the cleaner physical split
(markers literally at the identity site, dispatcher fully inside the runtime
file) at the cost of a disk-reading macro and the RA risk above.

[rust#54727]: https://github.com/rust-lang/rust/issues/54727
[rust#140514]: https://github.com/rust-lang/rust/pull/140514
[rust#99515]: https://github.com/rust-lang/rust/issues/99515
