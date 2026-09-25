# Addressing a peer you cannot depend on

> **Prerequisites:** the middle loop: you are writing wasm components with the
> actor SDK ([Writing a component](writing-a-component.md)). You know how a
> declared dependency works
> ([Declared dependencies](../systems/components.md#declared-dependencies)).

Two actors that mail each other in both directions cannot both address the
other by type. This recipe shows the shape that works: one actor declares the
other and announces itself, and the other keeps the envelope sender of that
announcement as its reference. The engine's editor shell and its regions are
the worked example throughout
([ADR-0141](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0141-editor-shell-input-ownership.md)).

## Why both directions cannot be declared

Three things close the obvious routes.

- **A declared dependency must be live before `init`.** `#[actor(depends(R))]`
  makes the load refuse unless `R` holds a `Live` route, with
  `"<actor> depends on <namespace>, which is not live"`. There is no ordering,
  retry, or wait, so two actors that declare each other both refuse. The
  bound behind it is `DependsOn` in `crates/aether-actor/src/model/mod.rs`.
- **Cargo refuses a crate cycle.** Addressing by type needs the other actor's
  type, so if each crate took the other as a normal dependency, the build
  fails.
- **Text is not a door.** `send_to_named` is gone
  ([ADR-0230](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0230-proven-actor-references.md)
  §5), and hand-folding a name with `mailbox_id_from_name` is disallowed in
  `clippy.toml`.

So one direction is a declared dependency and the other is a reference the
receiver was handed.

## Forward direction: a declared dependency

Pick one actor to be the dependent. It declares the other, and sends to it on
a ctx typed by itself. In the editor, the region declares the shell and
announces itself from `wire`
(`crates/aether-widget/src/editor_region.rs`):

```rust
#[actor(instanced, depends(EditorShell))]
impl WasmActor for EditorRegion {
    type Config = PanelConfig;
    const NAMESPACE: &'static str = "aether.widget.editor_region";

    // …

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_, Self>) {
        ctx.send::<EditorShell>(&RegionAttach { region: self.config.editor_region.clone() });

        // …
    }
}
```

`ctx.send::<R>(&kind)` is the flat send to a declared dependency
([ADR-0232](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0232-flat-ctx-send-verbs.md)).
It compiles only when the ctx's actor declares `R` (`A: DependsOn<R>`) and
`R` handles the payload's kind; the turbofish names only `R`
(`crates/aether-actor/src/wasm/ctx/send.rs`). To hold the dependency instead of
sending at once, `ctx.actor_ref::<R>()` mints an `ActorRef<R>` with no host
call, because the load already proved `R` live
(`crates/aether-actor/src/wasm/ctx/receive.rs`). Neither verb exists on the
erased ctx, so spell the ctx's actor as `Self`.

`R` must be keyless: a root singleton (`One`, like a chassis capability) or a
co-hosted peer (`Embedded`). The shell is a keyless singleton loaded under its
default name, which is what lets a region name it by bare type. Which resolver
an actor gets is
[ADR-0119](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0119-actor-addressing-via-a-resolver-strategy.md);
how the position follows the actor when it is re-parented is
[ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md).

### Across a crate boundary

In `aether-widget` both actors live in one crate, so the region names the
shell with a plain `use`. When the dependent lives in another component crate,
that crate depends on the receiver's crate with the receiver's `library`
feature, as `crates/aether-test-fixtures-bundle/Cargo.toml` does:

```toml
aether-widget = { path = "../aether-widget", features = ["library"] }
```

The receiver's crate declares the feature, non-default
(`crates/aether-widget/Cargo.toml`):

```toml
[features]
library = []
```

A wasm module carries exactly one `export!` expansion. Every item `export!`
emits is behind `cfg(not(feature = "library"))`, resolved against the crate
that invokes it, so `library` strips the receiver's entry surface and keeps its
actor impls linkable (the `library` section of the `export!` docs in
`crates/aether-actor/src/wasm/mod.rs`). The receiver's own wasm build never
enables the feature and keeps its entries.

The in-tree example is `EditorRegionProbe` in
`crates/aether-test-fixtures-bundle/src/editor_region_probe.rs`: a test fixture
in another crate that declares `depends(EditorShell)` and announces itself the
same way. The receiver may take the dependent crate back only as a
dev-dependency, which cargo allows because a dev-dependency edge is outside the
normal build graph; `aether-widget` does this to load the fixture in its
own tests, and the fixture manifest's comment says so.

## Reverse direction: the envelope sender

The receiver never declares the dependent. The dependent's announcement is an
ordinary kind the receiver handles (`RegionAttach`, which names only the
region), and the receiver's handler keeps the mail's sender
(`crates/aether-widget/src/editor.rs`):

```rust
#[handler::single]
fn on_region_attach(&mut self, ctx: &mut WasmCtx<'_>, attach: RegionAttach) {
    let Some(reference) = ctx.sender() else {
        tracing::warn!(/* … */ "region attach arrived with no sender; ignoring");
        return;
    };

    if !self.routing.attach(&attach.region, reference) {
        tracing::warn!(/* … */ "region attach names no unattached declared region; ignoring");
    }
}
```

`ctx.sender()` returns `Option<ErasedActorRef>`: a proof minted from the source
the host stamped on the envelope, with no lookup. It is `None` for a sourceless
dispatch (session, remote-engine, or broadcast mail), so the handler reports
that and returns. It needs no actor type, so it works on the erased ctx.
`Routing::attach` (`crates/aether-widget/src/routing.rs`) stores the
reference against the declared region name, and refuses an unknown name or a
second announcement for a region already attached rather than re-pointing a
live route.

Later pushes go through the stored reference with `ctx.send_to(reference,
&kind)`, as `EditorShell::forward` does for every input event it routes. An
`ErasedActorRef` is not kind-checked (an `ActorRef<R>` is), so the receiver
must send only kinds the announcing actor handles. The editor relies on that
by construction: `EditorRegion` handles each of the nine input kinds the shell
forwards.

A reply to the announcing mail itself needs no stored reference. The handler
replies, as any handler does.

### Load order

The dependency is `Live` before the dependent's `init`, so the announcement
from `wire` always has a live recipient. That fixes the load order: receiver
first, dependent second. `EditorRegion`'s own `# Agent` doc says the same:
load the shell first, and a region loaded before it is refused.

## Stored state holds proofs

Keep `ActorRef<R>` or `ErasedActorRef` in actor state, never a `MailboxId`.
The editor shell holds no address of its own: `Routing` stores the proof each
region handed over and gives that same value back as a route's target, so the
shell has nothing to resolve.

A position that arrives in a payload is proven once, at receipt. A native
actor does that with the ctx verb `resolve_live`
(`crates/aether-substrate/src/actor/native/ctx/address.rs`) and keeps the
proof, not the position. An address that arrives as an `ActorPath`, such as
the component path in a drop or replace request, is proven the same way
through `resolve_path`, whose refusal names the path, never a position. A guest has no such verb, so a guest keeps the
envelope sender instead of a payload-borne id.

No door turns a foreign `Address<R>` (one that arrived in mail, config, or
saved state) into a reference yet (ADR-0230 §3). The editor shell's
`RegionSpec.target` was the first site that would have needed one; issue
#6306 dropped the field instead, and the region announces itself.

## What not to write

- A string literal that repeats the other actor's `NAMESPACE` beside the send.
  `cargo xtask namespaces` reports a literal that repeats another crate's
  declared namespace.
- A hand-folded `MailboxId`.
- `depends` in both directions: both loads refuse.
- A normal dependency in both directions: cargo refuses the cycle.
- A payload field carrying the sender's position, re-resolved at every send.
  The kind names what the sender stands for (`RegionAttach` names the region);
  the envelope carries who sent it.

## Why capabilities never hit this

A capability crate exposes a marker face that compiles under
`default-features = false` ([capability anatomy](../capability-anatomy.md)).
Every component may depend on it, and no capability depends on a component, so
the graph has one direction by construction. Two user-space actors have no such
asymmetry until you choose one: the dependent declares, and the receiver keeps
the sender.

## Rejected: a hand-written marker in a shared kinds crate

Another shape puts a zero-sized marker per actor in a shared kinds crate and
addresses it by type from both sides. The engine does not provide it. Sending
to a marker by type compiles only if the marker implements `HandlesKind<K>`
for every kind sent to it, and `HandlesKind` is emitted by `#[actor]`; authors
never write it by hand (`crates/aether-actor/src/model/mod.rs`). The
struct-hosted identity form that gives capabilities their always-on marker is
native-only. A hand-kept marker would be a second naming and handled-kind
authority that nothing checks against the real actor.

## Where to read more

- [Declared dependencies](../systems/components.md#declared-dependencies) — the
  load-time refusal and what `depends` accepts.
- [Mail and kinds](../systems/mail-and-kinds.md) — sends, replies, and the
  proven references.
- [ADR-0230](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0230-proven-actor-references.md)
  — proven references and the doors that mint them.
- [ADR-0232](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0232-flat-ctx-send-verbs.md)
  — the flat ctx send verbs.
- [ADR-0141](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0141-editor-shell-input-ownership.md)
  — the editor shell and its regions.
