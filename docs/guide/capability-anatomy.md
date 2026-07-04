# Capability module anatomy

> **Governing ADRs:** [ADR-0121](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0121-capabilities-own-their-kinds.md)
> (a capability owns the kinds it exchanges with its callers) and
> [ADR-0122](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0122-split-actor-identity-from-runtime-state.md)
> (an actor's addressing identity is a separate type from its state-bearing
> runtime). This page states the shape those two decisions converge every
> capability in `aether-capabilities` on; it decides nothing new and cites
> the ADRs for the reasoning rather than re-deriving it.

Every native capability under `crates/aether-capabilities/src/` — `fs`,
`component`, `engine`, `audio`, `anthropic`, and the rest — is laid out the
same way. The shape below is that convention, grounded against the current
exemplars. A capability that deviates from it without a stated reason is
drifting; use this page, alongside `CLAUDE.md` and the two ADRs, as the
oracle for judging that drift. For *how to write* a capability, start from
[Adding a chassis capability](recipes/adding-a-chassis-capability.md) — that
recipe walks one exemplar end to end; this page is the catalog of rules
across all of them.

## 1. One directory per capability

A capability is a directory module, `src/<cap>/`, never a single file
(ADR-0121 decision 1). `fs/`, `component/`, `engine/`, `audio/`, and
`anthropic/` are each their own directory carrying a `mod.rs` plus however
many implementation files the capability's cohesion seams call for. A cap
never grows past a single `<cap>.rs` file — once it needs more than a
handful of items it becomes a directory instead, the same day it's added.

## 2. The identity ZST lives in `mod.rs`

A capability's addressing identity — the zero-sized `#[actor]` struct
carrying `Addressable` and the per-handler `HandlesKind` markers — is
declared in the module root, split from its state-bearing runtime
(ADR-0122). `crates/aether-capabilities/src/anthropic/mod.rs` states the
rule directly on `AnthropicCapability`:

> A ZST carrying only the addressing — `Addressable` (`NAMESPACE`,
> `Resolver`), the per-handler `HandlesKind` markers, and the
> name-inventory entry, all emitted always-on by `#[actor]`. The
> state-bearing runtime (`AnthropicCapabilityState`, which holds the
> `aether_substrate`-typed adapter + task queue) lives behind the one
> `feature = "runtime"` gate, so a transport-only build never names it nor
> pulls `aether_substrate` through this cap.

Two caps currently place the identity elsewhere — `input`'s
`InputCapability` lives in `subscription.rs`, and `gemini`'s `#[actor] impl`
sits in `capability.rs` rather than `mod.rs`. Both are variances the
sibling structure-alignment work (#2474/#2478) closes, not exemplars of
the rule.

## 3. Runtime tiers: a file when light, a directory when heavy

The runtime half — the state-bearing side gated behind `feature =
"runtime"` — is a single `runtime.rs` for a light cap and a `runtime/`
directory once it's heavy enough to decompose. The criterion is file
count and decomposition, not raw line count: `fs/runtime.rs` stays one
file despite its size, while `trampoline/runtime/`, `audio/runtime/`,
`component/runtime/`, `render/runtime/`, and `lifecycle/runtime/` are all
directories because each splits its runtime state across several
cohesion seams. `crates/aether-capabilities/src/trampoline/runtime/mod.rs`
states the criterion directly:

> The cap is heavy and already decomposed, so unlike `aether.fs`'s
> single-file `runtime.rs` the runtime half is a directory module […]

## 4. A cluster is a thin root plus per-actor subdirectories

A family of related capabilities under one parent root — several actors
sharing a mailbox surface — keeps the root `mod.rs` thin: a shared
`kinds.rs`, and one subdirectory per actor. `engine/` is the exemplar
(`mod.rs` at 23 lines, `kinds.rs`, `proxy/`, `server/`, `store/`); `http/`
is the other (`mod.rs` at 31 lines, `kinds.rs`, `client/`, `server/`). The
root's job is composition — re-exports and module declarations — not
implementation.

`tcp/mod.rs` is 645 lines and has not been thinned to this shape yet; it's
the cluster converging toward it, not a finished exemplar of it — sibling
#2477 does the thinning.

## 5. The wholly-native LLM sub-shape

The content-generation capabilities — `anthropic`, `gemini`, and their
shared `contentgen` machinery — are module-gated once at `lib.rs` behind
`#[cfg(not(target_family = "wasm"))]`, and their backend files sit flat
inside the module rather than in a per-backend subdirectory:
`anthropic/{api,cli}.rs`, `gemini/{adapter,lyria,nanobanana}.rs`,
`shared/contentgen/{adapter,shared,staging,task_queue}.rs`. These caps make
blocking HTTPS / subprocess calls no wasm guest ever needs to address by
type, so they skip the wasm-safe marker layer entirely rather than
carrying an always-on identity split most of the rest of the crate does.

## 6. The cfg-gate ladder

Three gates stack, each with a stated reason:

- **`feature = "runtime"`**, on by default for a chassis build
  (`default = ["runtime"]` in `Cargo.toml`), gates the state-bearing
  runtime half — the `Lifecycle` / `Dispatch` / `NativeActor` impls and
  every `aether_substrate`-typed field.
- **`<cap>-runtime` features** (`audio-runtime`, `render-runtime`) gate
  the media capabilities whose runtime pulls a heavy native-only
  dependency — `cpal` for audio, `wgpu` for render — on top of the base
  `runtime` gate.
- **`#[cfg(not(target_family = "wasm"))]`** is used sparingly and only
  with a comment stating why. `lib.rs`'s module ladder carries one on
  each wholly-native module (`anthropic`, `gemini`, `shared`,
  `transforms`) explaining the native-only dependency it elides on a
  wasm build. A narrower case is `input/subscription.rs`, where the
  `SubscribeInputResult` reply-kind import alone rides
  `#[cfg(not(target_family = "wasm"))]` rather than the full `runtime`
  gate — the `#[actor]` macro's ADR-0109 `HandlerEntry` inventory
  submission runs on every native build, runtime feature or not, and
  needs the reply kind's `::ID` even on a transport-only build.

## 7. A kind that stays upstream cites its must-stay rule

A capability owns the kinds it exchanges with its callers in its own
`kinds.rs` (ADR-0121 decision 2). Where some of a cap's kinds stay in
`aether-kinds` instead — because a consumer that can't depend on the
capability needs them (the substrate core, or the MCP harness) — the
cap's `mod.rs` documents which kinds stay upstream and for whom.
`crates/aether-capabilities/src/render/mod.rs` (lines 17–23) is the
model:

> The cap's drawing + texture mail kinds live in `kinds` (ADR-0121): they
> ride the always-on (marker-only `render`) region so a wasm guest sees
> the kind types for typed addressing without the `render-runtime` GPU
> stack. The capture-request and `FrameCheck` verification kinds stay in
> `aether-kinds` (consumed upstream by `aether-mcp` and the substrate
> core), as do the `QuadSpace` / `QuadScale` projection types the
> `aether.text` kinds share.

## 8. Test placement

Inline `#[cfg(test)] mod tests` at the end of the file under test is the
default. A sibling `tests.rs` is for when the tests dwarf the subject —
`engine/store/tests.rs` and `rpc/server/tests.rs` are both this case.
Fixtures shared across capability families are private crate-root
modules declared `#[cfg(test)] mod` in `lib.rs`: `test_chassis.rs` and
`test_echo.rs`. `lib.rs` states why `test_echo` sits at the crate root
rather than under one capability:

> Shared round-trip test scaffolding (echo actor + its kinds), used by
> the `rpc::server` test modules and the `engine::proxy` test. Lives at
> the crate root — not under `rpc` — because its consumers span
> families; a private top-level `mod` is reachable crate-wide via module
> privacy (the `test_chassis` pattern).

## Two open decisions

The structure review that grounded this page also surfaced two small
places where the caps have converged on more than one accepted answer.
Both stand as recorded decisions rather than open questions to resolve.

### Empty-config philosophy

A config-free capability has two accepted shapes. `input`'s `InputConfig`
(`input/config.rs`) keeps an empty struct:

```rust
#[derive(Default)]
pub struct InputConfig {}
```

so the chassis composes it with the same `Builder::with_actor::<InputCapability>(InputConfig {})`
shape every other cap uses, and a future knob lands without changing the
call site. `text`'s runtime (`text/runtime/mod.rs:247`) instead sets
`type Config = ();` — there's nothing on the horizon this cap would ever
need to configure.

Reach for the empty-struct form when a config knob is plausible later and
you want the composition site stable across that addition; reach for `()`
when the capability genuinely has nothing to configure, now or
foreseeably.

### Fail-fast stub-cap naming

A chassis without a subsystem still needs to claim that subsystem's
mailbox, so it composes a stub capability in its place. Two naming
conventions cover this: `Headless*Capability` for the chassis-without-
that-subsystem companion — `HeadlessRenderCapability`
(`render/mod.rs:279`), `HeadlessWindowCapability` (`window/mod.rs:35`) —
and `UnsupportedTestBenchCapability` (`test_bench.rs:36`) for the
test-bench chassis's blanket stand-in.

`Headless*` names the specific subsystem the chassis is missing (render,
window) and answers requests with `Err` in a way that mirrors the real
cap's reply shape. `UnsupportedTestBenchCapability` covers whichever
mailboxes the test bench doesn't stand up at all, under one name rather
than one `Headless*` type per absent subsystem.

## Where to read more

- The step-by-step version of rules 1–3 — writing a new capability from
  scratch — [Adding a chassis capability](recipes/adding-a-chassis-capability.md).
- The identity/runtime split's full reasoning and alternatives —
  [ADR-0122](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0122-split-actor-identity-from-runtime-state.md).
- The kind-ownership decision and its must-stay rules — [ADR-0121](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0121-capabilities-own-their-kinds.md).
- The pub-or-private visibility convention this crate also enforces —
  `CLAUDE.md`.
