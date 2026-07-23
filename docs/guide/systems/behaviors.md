# Behaviors and in-cluster scripting

A behavior is small wasm code injected at a position in a running guest actor
tree. It transforms mail already flowing through that position. It is not a
component with a tiny API: it owns no mailbox, declares no kinds, and runs
inside a `BehaviorHost` actor.

## Where behaviors fit

```text
component load        runtime code + its own actor environment
inline actor child    load-time code + a shared component environment
behavior script       runtime code + a shared position in an actor tree
```

The forcing use case is widget glue: clamp a value, react to a click, or emit a
small effect without rebuilding and replacing the entire component. The host is
generic actor infrastructure; widgets are the first consumer.

Use a behavior when all required input kinds already flow past the chosen tree
slot. Graduate to a component when code needs its own vocabulary, mailbox,
subscriptions, wider authority, or fail-closed semantics.

## The two wasm artifacts

The behavior tier deliberately separates:

- a **script**, built as a `cdylib` against `aether-behavior` and not
  `aether-actor`;
- a **host-enabled component**, built with the behavior host feature and carrying
  the `wasmi` interpreter plus `BehaviorHost` export.

Structural build discovery uses that dependency distinction. A script that
depends on `aether-actor` is classified as a component, so do not share a guest
component crate into the script merely to reuse kind types. A script commonly
declares local wire-compatible twins of the kinds it intercepts.

## Interposition is tree position

`BehaviorHost` is an instanced wasm actor. It occupies a child slot and spawns
the real child beneath it:

```text
parent
  └─ BehaviorHost
       └─ wrapped actor
```

Down-lane mail reaches the host, is offered to the script, then forwards to the
child. Up-lane mail from the child passes through the host on its way to the
parent. Stacking hosts creates explicit tree-order precedence.

A component root has no implicit slot above it. Root interception requires the
component to self-embed/reroute through a host; there is no hidden membrane hook
that bypasses normal actor tracing.

## Filter contract

The authoring macros express intent in the handler signature:

- `#[on]` with `&K` observes and forwards the original mail;
- `#[on]` with `&mut K` may mutate the value that forwards;
- `ctx.consume()` drops the in-flight item;
- widget/child/panel handles record effects that the host sends after the
  verdict;
- `#[on_attach]`, `#[on_frame]`, and `#[on_detach]` cover lifecycle seams.

The host/script envelope encodes a `Verdict` (`Forward(bytes)` or `Consume`)
plus an ordered effect list. The verdict applies first; effects drain afterward
in recorded order. Scripts address targets by relative role/subname, never raw
mailbox id.

## Fail-open firewall

Every filter call gets a fresh fuel budget. A trap, bad decode, missing export,
or fuel exhaustion forwards the mail unmodified and logs the failure. After a
configured number of consecutive traps, the host disables the script into
passthrough until replacement.

This makes a behavior unsuitable as a security or correctness gate. If traffic
must be blocked even when extension code fails, put the policy in a real actor
or trusted capability.

## Discovery and cost

The script exports a custom manifest listing the kind ids it handles. The host
skips the interpreter for undeclared kinds. Scripts do not register new kinds,
so `describe_kinds` remains the complete engine vocabulary.

The script is not a mailbox node in settlement or tracing. Its work is charged
inside the host handler and is visible through that host's `actor_cost` and
logs. Script-level observability, if expanded, belongs in host instrumentation.

## Mirrors, effects, and state

Behavior reads are copies in script memory. The SDK keeps last-value-per-kind
mirrors from traffic already flowing through the host; `report()` asks a target
to re-emit observable state rather than exposing its fields.

Writes are effects projected back into ordinary mail. Best-effort echo
suppression prevents a script's own set from immediately looping, but a target
that normalizes bytes may legitimately re-offer the result.

Authored script state is serialized through `state_save`/`state_load` and can be
offered to a replacement script. Mirrors, tree caches, and pending effects are
derived state and rebuild after attach. The wrapped actor retains ownership of
its own persisted state.

## Loading and swapping

`HostConfig` names the wrapped child, initial `ScriptSource`, fuel/trap limits,
frame trigger, and always-mirrored kinds. A source may be absent, inline wasm,
or an `aether.fs` namespace/path.

At runtime the host accepts:

| Kind | Action | Reply |
|---|---|---|
| `aether.behavior.set_script` | validate and swap inline wasm bytes | `aether.behavior.load_script_result` |
| `aether.behavior.load_script` | fetch bytes from an FS namespace and swap | same result after the read settles |

Failed swaps preserve the prior running script. An OK result reports resident
byte length.

The worked authoring path is [Writing a behavior](../recipes/writing-a-behavior.md).

## Change route

- Script SDK, ABI, manifest: `crates/aether-behavior/src/{runtime,abi,manifest,envelope}.rs`
- Derives: `crates/aether-behavior-derive/`
- Host actor/config/persistence: `crates/aether-behavior/src/host/`
- Widget composition adapter: `crates/aether-kit-widget/src/`
- Build discovery: `xtask/src/inventory.rs`
- Decision: accepted ADR-0137
