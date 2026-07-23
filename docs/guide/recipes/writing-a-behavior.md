# Writing a behavior

> **Prereqs:** `cargo` with the `wasm32-unknown-unknown` target to compile your
> script, and the [MCP harness](../mcp-harness.md) up to attach and drive it. A
> behavior is a small wasm script the engine interprets in place, so you author
> and iterate on it the same way you drive the live engine — compile the script,
> point a host at it, watch it transform mail.

A behavior transforms the mail already flowing past a position in a running actor
tree — it declares no kinds, owns no mailbox, and is interpreted by a host actor
that occupies a slot and offers each passing mail to your script. This recipe
takes an empty crate to a script a host runs: crate setup, the `#[behavior]`
block, the wasm build, and attaching it. Read [Writing guest code](../writing-guest-code.md)
first for when to reach for a behavior instead of a component; this recipe assumes
you've made that call.

> **Verify against current code.** This recipe carries symbol names and file
> paths — confirm them before following it. The script SDK is
> `crates/aether-behavior` (its `runtime` module: `Behavior`, `BehaviorCtx`, and
> the widget/child/panel handles); the `#[behavior]` macro is
> `crates/aether-behavior-derive`; the host and its config vocabulary are
> `crates/aether-behavior/src/host/`. If a name below has moved, the fix is part
> of the work.

## 1. Set up the crate

A behavior script is a `cdylib` that depends on `aether-behavior` and **not** on
`aether-actor`. That absence is load-bearing: a component is discovered
structurally by its `aether-actor` dependency, so a script tree that stays clear
of `aether-actor` never classifies as a component. It also means a script cannot
share the kind crate a component would — anything that pulls `aether-actor`
transitively (`aether-kit-widget`, for one) is off-limits.

```toml
# Cargo.toml
[package]
name = "clamp-behavior"
version = "0.0.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]          # the wasm script artifact

[dependencies]
aether-behavior = { path = "../aether-behavior" }             # SDK (runtime face, on by default)
aether-data = { path = "../aether-data", default-features = false, features = ["derive"] }
serde = { version = "1", default-features = false, features = ["alloc", "derive"] }
```

Because a script can't depend on the crate that defines the kinds it transforms,
declare a **local twin** of each kind under the *same* `#[kind(name = "…")]` wire
name. The name is what the `KindId` and wire bytes derive from, so a twin with the
matching name decodes the real traffic byte-for-byte. Copy the field shape exactly
— a drift between your twin and the real kind is a decode mismatch at runtime, not
a compile error.

```rust
use serde::{Deserialize, Serialize};

// Twin of aether-kit-widget's slider event — same wire name, so it decodes the
// real SliderChanged flowing past the host.
#[derive(Serialize, Deserialize, aether_data::Kind, aether_data::Schema, Clone)]
#[kind(name = "aether.kit.widget.slider.changed")]
struct SliderChanged {
    value: f32,
    committed: bool,
}
```

## 2. Write the `#[behavior]` block

The receive side is one `#[behavior] impl Behavior for C` block — the `#[actor]`
shape, one tier over. Each `#[on]` method is a handler; the macro infers the kind
from the method's third parameter, and the parameter's mutability *is* the intent:
`&mut K` intercepts (the mutated value re-encodes and forwards), `&K` observes.
`ctx.consume()` drops the in-flight mail. There is no verdict type — mutation and
`consume` are the whole vocabulary.

```rust
use aether_behavior::BehaviorCtx;
use aether_behavior::behavior;
use serde::{Deserialize, Serialize};

// Authored state: the clamp cap and how many commits we've clamped. The
// struct's fields ARE the persisted state (serde, carried across a swap).
#[derive(Default, Serialize, Deserialize)]
struct Clamp {
    cap: f32,
    clamped: u32,
}

#[behavior]
impl Behavior for Clamp {
    // Runs once the mirrors are primed and ctx is live. Seed defaults here.
    #[on_attach]
    fn attached(&mut self, _ctx: &mut BehaviorCtx) {
        if self.cap == 0.0 {
            self.cap = 0.8;
        }
    }

    // Intercept: `&mut SliderChanged`. The mutated event forwards on; a
    // consumed one does not.
    #[on]
    fn on_change(&mut self, ctx: &mut BehaviorCtx, m: &mut SliderChanged) {
        if !m.committed {
            ctx.consume(); // drop uncommitted drag noise — it never forwards
            return;
        }
        if m.value > self.cap {
            m.value = self.cap; // clamp; the forwarded event carries 0.8
            self.clamped += 1;
        }
    }
}
```

Three parts earn attention:

- **The macro provides the trait and the markers.** `#[behavior]` rewrites the
  block to `impl aether_behavior::Behavior`, so you neither import `Behavior` nor
  the `#[on]` / `#[on_attach]` markers — writing them is enough. The only imports
  a minimal script needs are `BehaviorCtx` (named in handler signatures) and the
  `behavior` attribute itself.
- **State is your struct.** The fields you derive `Serialize` / `Deserialize` on
  are what persists — the macro's default `state_save` / `state_load` serialize
  the whole struct, and a script swap offers the old blob to the replacement. A
  `Default` impl is required (the host constructs the instance lazily). Override
  `state_save` / `state_load` only for a custom migration.
- **Reads and effects go through `ctx`.** `ctx.widget().last::<K>()` reads the
  last value of a kind that flowed past this slot (a decode-once mirror, not a
  request); `ctx.widget().set(&k)`, `ctx.panel().emit(&k)`, and
  `ctx.child(path).send(&k)` project effects that the host drains into real
  cluster sends after your handler returns. A per-frame hook is `#[on_frame]`, and
  teardown is `#[on_detach]`.

## 3. Build for wasm32

A script is a std `cdylib` — std supplies the allocator and panic handler, so
there is no `#![no_std]` ceremony.

```console
$ rustup target add wasm32-unknown-unknown    # once per toolchain
$ cargo build --target wasm32-unknown-unknown --release -p clamp-behavior
```

The artifact lands at
`target/wasm32-unknown-unknown/release/clamp_behavior.wasm` (dashes turned to
underscores). Release keeps it small — this one is ~50 KiB. The macro emits the
guest exports (`filter`, `alloc`, `state_save`, `state_load`) and an
`aether.behavior.exports` custom section listing the kind ids your handlers cover,
which lets the host skip the interpreter for traffic your script doesn't touch.

## 4. Attach it to a host

A behavior runs inside a **`BehaviorHost`** (`aether.behavior.host`) — a stock
actor that wraps one child, occupies its slot, and offers the slot's mail to your
script. The host takes a `HostConfig` naming the wrapped child (by its actor type
tag), the script source, and the fuel budget. Encode that config with the real
types — `aether_kit_widget::WidgetKind::type_tag()` yields a stock widget's tag
without linking that crate's runtime, and `ScriptSource::Inline` ships the wasm bytes
directly. The following is a small **host-side config-preparation program**, not
code linked into the behavior script:

```rust
use aether_behavior::host::{ChildSpec, HostConfig, ScriptSource};
use aether_kit_widget::{SliderConfig, Theme, WidgetKind};
use aether_data::Kind;

let script = std::fs::read(".../clamp_behavior.wasm")?;
let slider = SliderConfig {
    min: 0.0,
    max: 1.0,
    step: 0.0,
    initial: 0.5,
    theme: Theme::DEFAULT,
    state: Default::default(),
};
let config = HostConfig {
    child: ChildSpec {
        type_tag: WidgetKind::Slider.type_tag().unwrap(),
        subname: "slider".into(),
        config: slider.encode_into_bytes(),
    },
    script: ScriptSource::Inline(script),
    fuel_per_call: HostConfig::DEFAULT_FUEL_PER_CALL,
    disable_after_traps: HostConfig::DEFAULT_DISABLE_AFTER_TRAPS,
    frame_trigger: 0,        // no per-frame hook
    mirror_kinds: Vec::new(),
};
std::fs::write(".../host_config.json", serde_json::to_vec_pretty(&config)?)?;
```

Load the host from an `aether-kit-widget` build compiled with its `behavior`
feature (which pulls the interpreter in and exports the host), pointing
`config_path` at that JSON:

```text
upload_component(staged_path = ".../aether_kit_widget.wasm")             → { hash, name }
spawn_substrate()                                                        → engine_id
load_component(engine_id, selector, export = "aether.behavior.host",
               config_path = ".../host_config.json")                     → LoadResult
```

`config_path` is always JSON. The harness schema-encodes it into the component's
config kind. Passing `HostConfig::encode_into_bytes()` here is incorrect because
that would ask the harness to parse binary wire bytes as JSON. For hand-authored
inline config, a `Bytes` field can use `{"$file": "..."}` so the MCP front reads
the file without expanding it into a JSON integer array; verify the exact enum
shape with `describe_component`/`describe_kinds`.

`LoadResult::Ok` means the host came up: the wrapped slider spawned as its inline
child, and the interpreter instantiated your script. The host now interposes on
the slot — mail addressed to it is offered to your `#[on]` handlers before it
forwards to the wrapped child.

In a widget panel you rarely build a `HostConfig` by hand: the panel's declarative
child specs carry a
`WidgetKind::BehaviorHost` slot whose config is a `BehaviorHostSpec` (the wrapped
widget kind, its config, and the script), and the panel builds the `HostConfig`
for you. Keep the wrapped config opaque but complete: encode the stock config's
defaulted `WidgetControlState` alongside its value/theme fields. The panel
decodes `BehaviorHostSpec.wrapped` plus `wrapped_config` to derive the same row
height, pointer eligibility, focusability, and initial availability it would
derive for an unwrapped child. The direct load above is the mechanism under that
convenience.

## 5. Swap the script live

The host takes two control kinds so you iterate without reloading it. Rebuild the
wasm, then swap it in place:

- `aether.behavior.set_script { bytes }` replaces the running script with inline
  wasm and replies `load_script_result` (`Ok { resident_bytes }` / `Err`) — the
  fast edit loop.
- `aether.behavior.load_script { namespace, path }` fetches the replacement from
  an `aether.fs` namespace instead.

A swap carries your authored state forward: the old script's `state_save` blob is
offered to the replacement's `state_load`, so a `Clamp` swapped for a new build
keeps its `clamped` count.

## The firewall: a behavior is not a gate

Every filter call runs under a fuel budget, and on any fault — fuel exhaustion, a
bad decode, a trap — the host **fails open**: the in-flight mail forwards
untransformed, the fault is logged, and after `disable_after_traps` consecutive
faults the script drops to a passthrough until you replace it. A degraded-but-alive
widget is the correct residue of a broken script. The direct consequence: logic
that must *block* traffic to be correct cannot be a behavior, because a fault would
let that traffic straight through. Gate-shaped logic belongs in a component — see
[Writing guest code](../writing-guest-code.md#a-behavior-is-not-a-gate).

## Where to read more

- When to write a behavior versus a component, and the graduation path between
  them — [Writing guest code](../writing-guest-code.md).
- The mechanism: interposition as tree position, the mirror-and-effects model, the
  fail-open firewall, and the vocabulary boundary —
  [ADR-0137](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0137-in-cluster-behavior-script-host.md).
