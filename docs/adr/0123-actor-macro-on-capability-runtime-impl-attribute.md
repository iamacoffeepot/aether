# ADR-0123: Actor macro on the capability, runtime impl attribute

- **Status:** Proposed
- **Date:** 2026-06-24

## Context

ADR-0122 split a native capability into an always-on **identity** (a ZST carrying the addressing surface — `Addressable`, the per-handler `HandlesKind<K>` markers, the name-inventory entry) and a feature-gated **runtime** (the `State` struct plus the substrate-typed behavior). A marker-only or wasm build sees the typed-send surface — `ctx.actor::<Cap>().send(&kind)` compiles — without dragging in the substrate/GPU/audio stack.

The split as shipped places the `#[actor(runtime_feature = …)] impl NativeActor for Cap` block — the handler bodies and the dispatch table — in the identity file (`mod.rs`), and puts only the `State` struct in `runtime.rs`. So the behavior sits apart from the state it drives, and the identity file carries the heavy dispatch surface. The macro divides its own output by cfg (markers ungated, runtime impls gated), but both halves are emitted at the one site where the impl is written.

We want the behavior to live with its state in the runtime module, `#[actor]` to sit on the capability itself (the struct *is* the actor), and the always-on identity to remain reachable even when the runtime module is `#[cfg]`-stripped.

Two constraints shape the whole design:

- A proc-macro emits only at its invocation site. One invocation cannot put markers in `mod.rs` and a dispatcher in `runtime.rs`. The marker set is derived from the handler list, so the macro that emits the markers must see the handlers.
- An attribute macro cannot sit on a file module declaration: file modules in proc-macro input are unstable (rust#54727 — inline `mod m { … }` is stable, `mod m;` is not). So `#[actor] mod runtime;` is rejected on stable.

## Decision

Author a split capability as two attributes that share the handler set by reading it off disk:

- **`#[actor(<cardinality>[, <module>])]` on the capability struct.** It reads the sibling runtime module file (default `runtime`, overridable by the second argument), parses it, and emits the always-on identity against the struct: `impl Addressable` (the `NAMESPACE` and the cardinality `Resolver`), one `impl HandlesKind<K>` per `#[handler]`, and the name-inventory entry. The struct restates nothing — both the kinds and the namespace are read from the runtime impl.

- **`#[runtime]` on the `impl NativeActor for Cap` block** in the runtime module. It emits the gated runtime surface — `Lifecycle`, `Dispatch`, the `NativeActor` composition that pins `type State`, and the handler bodies as an inherent impl — and consumes the `NAMESPACE` const (declared once here, lifted into `Addressable` by `#[actor]`). It rides the module's `#[cfg]`.

- **`mod runtime;` stays a plain, author-written, `#[cfg(feature = "…")]`-gated line** that no macro touches.

The disk read uses `proc_macro::Span::local_file()` (stable since Rust 1.88) to resolve the file holding the `#[actor]` invocation; the sibling runtime file is read and parsed cfg-agnostically, so the identity is harvested even in a configuration where `mod runtime` is stripped. The `NAMESPACE` const is the single declaration site for the cap's name; cardinality (`singleton` / `instanced`) stays on `#[actor]` because it is pure identity (the `Addressable::Resolver`).

The existing impl-hosted `#[actor] impl WasmActor/NativeActor for X` form stays valid for wasm guests and un-split caps; this decision adds the struct-hosted form for split native caps.

A producible proof exists on branch `spike/actor-pull-up`: a `pull-up-macro` (`#[actor]` + `#[runtime]`) and a `consumer` crate that builds and tests with the runtime feature on and off, clippy-clean, on the pinned 1.96 toolchain.

## Consequences

- Behavior and state cohere in the runtime module; the identity file is the ZST plus the gated `mod runtime;`. `#[actor]` on the capability reads as a statement of what the struct is.
- The identity carries no restated kind list or namespace — both are read from the impl, so the anti-drift property the auto-marker design protects is preserved.
- The macro now reads a source file from disk at expansion. The recorded risks:
  - **IDE dependency.** rust-analyzer and RustRover must run the macro and honor `local_file()` for the lifted identity to resolve in-editor. If an engine stubs `local_file()` or skips the read, typed sends light up red in the IDE even though `cargo` is green. This is the gating risk and is assumed working pending in-editor verification; if it proves untenable, the no-macro alternative below is the fallback.
  - **Path remapping.** `local_file()` returns `None` under `--remap-path-prefix`; the macro needs a defined fallback (a hard error with a clear message at minimum).
  - **Verbatim tokens.** Lifted kind tokens are emitted as written, so handler argument types must resolve in the identity module's scope — write them bare, not `super`-qualified.
  - **cfg-blind harvest.** The parse ignores cfg, so a cfg'd-out handler is still lifted.
  - **Incremental compilation.** In a runtime-*on* build the runtime file is a real compiled crate module, so editing it changes the crate fingerprint and forces a recompile that re-runs the macro — no extra dependency edge needed there. This does *not* hold in a runtime-*off* (transport-only) build: `mod runtime;` is cfg-stripped, so it is not a compilation input, yet the harvest still reads it off disk — an edit to the runtime file would not re-fingerprint the crate and stale markers could survive an incremental rebuild. The macro therefore emits an ungated `const _: &[u8] = include_bytes!(<absolute runtime path>);` alongside the addressing markers, creating a real compile-time edge on the file it read; `include_bytes!` is the stable substitute for the unstable `tracked_path` API (the crate is stable-pinned).
- Follow-on work: the macro support in `aether-actor-derive`; a reference migration (`tcp`, the smallest split cap, covered by FleetBench); per-cap migrations of the remaining split caps (`render`, `audio`, `text`, `ui`, `fs`, `component`, `lifecycle`, `anthropic`, `gemini`); then flipping this ADR to Accepted.

## Alternatives considered

- **Attribute on the file module — `#[actor] mod runtime;`.** Rejected: file modules cannot be proc-macro input (rust#54727).
- **Function-like `lift_markers!(runtime => Cap)` in the identity file.** Works on stable (the file module is neither input nor output), but reads as boilerplate and restates the identity name; the struct-hosted attribute subsumes it.
- **No macro (leave the split where ADR-0122 put it).** Leave `mod runtime;` ungated and gate only the substrate-typed items inside it, letting the existing `#[actor(runtime_feature = …)]` emit markers ungated from within the runtime file — crate-global impls reach the identity for free. Needs no new machinery and is IDE-clean, but the dispatch and the markers stay co-located in the runtime file and the identity file never carries `#[actor]`. This is the fallback if the IDE dependency proves untenable.
- **Hoist `NAMESPACE` onto the `#[actor]` args instead of reading the const.** Viable, but reading it from the impl keeps the struct dumber and the name declared once where the behavior lives. Cardinality still rides `#[actor]` because it is identity, not behavior.
