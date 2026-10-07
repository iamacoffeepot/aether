# Addressing a peer you cannot depend on

> **Prerequisites:** the middle loop: you are writing wasm components with the
> actor SDK ([Writing a component](writing-a-component.md)). You know how a
> declared dependency works
> ([Declared dependencies](../systems/components.md#declared-dependencies)).

Two actors that mail each other in both directions cannot both address the
other by type. This recipe shows the shape that works: one actor declares the
other and announces itself, and the other keeps the envelope sender of that
announcement as its reference. No pair in the tree does both halves at once,
so the forward direction is shown as a sketch of two actors, a `Mixer` and
the `Channel`s that attach to it, and the reverse direction and the
cross-crate dependency are shown from `aether-kit` and `aether-demo`.

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
  §5), and hand-hashing a name is closed: the name hashes are private to
  `aether-data`, and `MailboxId::from_name` is disallowed in `clippy.toml`.

So one direction is a declared dependency and the other is a reference the
receiver was handed.

## Forward direction: a declared dependency

Pick one actor to be the dependent. It declares the other, and sends to it on
a ctx typed by itself. In the sketch, the channel declares the mixer and
announces itself from `wire`:

```rust
#[actor(instanced, root, depends(Mixer))]
impl WasmActor for Channel {
    type Config = ChannelConfig;
    const NAMESPACE: &'static str = "example.channel";

    // …

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_, Self>) -> Result<(), ActorInitError> {
        ctx.send::<Mixer>(&ChannelAttach { channel: self.config.channel.clone() });

        // …
        Ok(())
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

`R` must be a root singleton (`One`), a chassis capability or a loaded guest
alike. The mixer is a singleton guest named at the root by its published
namespace
([ADR-0241](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0241-code-is-published-not-loaded.md)
§5), which is what lets a channel name it by bare type. Which resolver
an actor gets is
[ADR-0119](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0119-actor-addressing-via-a-resolver-strategy.md);
how the position follows the actor when it is re-parented is
[ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md).

### Across a crate boundary

When both actors live in one crate, the dependent names the receiver with a
plain `use`. When the dependent lives in another component crate, that crate
depends on the receiver's crate with the receiver's `library` feature, as
`crates/aether-demo/Cargo.toml` does:

```toml
aether-kit = { path = "../aether-kit", features = ["library"] }
```

The receiver's crate declares the feature, non-default
(`crates/aether-kit/Cargo.toml`):

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

The in-tree example is `Demo` in `crates/aether-demo/src/lib.rs`: a component
in another crate that declares `depends(MeshViewer, RenderCapability)` and
mails the viewer from `wire` with `ctx.send::<MeshViewer>(..)`. The receiver
may take the dependent crate back only as a dev-dependency, which cargo allows
because a dev-dependency edge is outside the normal build graph.

## Reverse direction: the envelope sender

The receiver never declares the dependent. The dependent's announcement is an
ordinary kind the receiver handles, and the receiver's handler casts the mail's
sender to the protocol it will send and keeps the result. The camera in
`aether-kit` does this for whoever subscribes to its view: the announcement is
`aether.render.view_subscribe`, which carries nothing, and the camera keeps
its sender (`crates/aether-kit/src/camera/mod.rs`):

```rust
#[handler::tell]
fn on_view_subscribe(&mut self, ctx: &mut WasmCtx<'_>, _subscribe: ViewSubscribe) {
    let Some(sender) = ctx.sender() else {
        tracing::warn!(target: "aether_kit", "view subscribe arrived with no sender; ignoring");
        return;
    };
    let Some(viewer) = ctx.cast::<Subscriber<ViewProjection>>(sender) else {
        tracing::warn!(target: "aether_kit", "view subscribe sender does not take a view projection; ignoring");
        return;
    };

    let watch = ctx.watch(viewer, NoContext);
    self.viewers.add(viewer, watch);
    // …
}
```

`ctx.sender()` returns `Option<ErasedActorRef>`: a proof minted from the source
the host stamped on the envelope, with no lookup. It is `None` for a sourceless
dispatch (session, remote-engine, or broadcast mail), so the handler reports
that and returns. An erased reference has no send verb
([ADR-0231](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0231-protocol-typed-references-and-reply-checks.md)
§4), so the handler casts it once, at receipt, to
`Subscriber<ViewProjection>`, the protocol of an actor that takes
`aether.view_projection` and answers nothing. The cast answers `None` when
the sender's published rows do not cover the protocol, and the handler refuses
that subscription. `Viewers::add` (`crates/aether-kit/src/camera/viewers.rs`)
stores the typed reference and the id of the watch on it, keyed by the
reference's erased twin, so a second subscription from the same actor is held
once. The camera watches the cast reference so that a subscriber that closes
without unsubscribing is removed
([Watching another actor](../systems/components.md#watching-another-actor)).

Later pushes go through the stored `ProtocolRef<Subscriber<ViewProjection>>`
with `send_to(reference, &kind)`, as `Viewers::send` does for every view the
camera publishes, and each send is kind-checked against the protocol's rows.
The mixer in the sketch does the same with its own protocol: its
`ChannelAttach` handler casts the sender to the protocol of the kinds it
pushes to a channel and keeps the result against the channel's name.

Under the design rules
([R-0044](../contributing/design-rules.md#r-0044),
[R-0040](../contributing/design-rules.md#r-0040)), a receiver holds a
`ProtocolRef<P>` of the protocol the dependent speaks, which it gets one of
two ways: by casting the envelope sender at receipt, as the camera does, or by
proving a typed path (an `ActorPath<R>` or a `ProtocolPath<P>`) that the
announcement carries. A guest has all three: the cast, `WasmCtx::cast`, and
`WasmCtx::resolve` over either typed path, since a guest decodes a
`ProtocolPath<P>` as a native actor does (#7205, #7501).

A reply to the announcing mail itself needs no stored reference. The handler
replies, as any handler does.

### Load order

The dependency is `Live` before the dependent's `init`, so the announcement
from `wire` always has a live recipient. That fixes the load order: receiver
first, dependent second. In the sketch the mixer is loaded first, and a
channel loaded before it is refused.

### Talking back to the actor that loaded you

No door proves the actor that loaded a component, so the loader hands the
component its reference. A guest's `ctx.parent()` exists only for an actor
that declares `child_of(..)`, and answers only inside its own module: it finds
the inline parent in the module's cluster. A root-only entry actor has no
`ctx.parent()` at all, and one that is both `root` and a child gets `None`
when it is placed at the root. The entry actor's lineage parent is the component
host, which spawned the component's trampoline, not the actor that sent
`aether.component.load`, and no ctx proves either
([ADR-0230](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0230-proven-actor-references.md)
§3).

Two shapes reach the loader, and both reuse doors above:

- **The loader mails first.** `LoadResult::Ok` is sent by the loaded actor
  itself, so the loader keeps that reply's `ctx.sender()` as its proof of the
  component and mails the component through it. The component keeps
  `ctx.sender()` from that mail, as in the reverse direction above.
- **The loader's path rides in config.** The loader puts its own path in the
  component's config, and the component's `wire` proves it once and keeps
  the proof.

Either way the component sends to the loader only through a typed proof,
since an erased reference has no send verb. A reference kept from
`ctx.sender()` is cast to a `ProtocolRef<P>` at receipt, as in the reverse
direction above. The loader's config field is a typed path, an
`ActorPath<R>` or a `ProtocolPath<P>` by what the component needs, and the
component stores the typed proof `ctx.resolve` returns for it.

## Stored state holds proofs

Keep proofs in actor state, never a `MailboxId`. For an actor the state will
send to, the proof is typed: an `ActorRef<R>` or a `ProtocolRef<P>`
([R-0044](../contributing/design-rules.md#r-0044)). An `ErasedActorRef` is
kept only where nothing is sent through it: comparing identity, keying a
table, naming a path, or monitoring. The camera holds no address of its own:
`Viewers` stores the proof each subscriber handed over, cast once to
`Subscriber<ViewProjection>`, beside the id of the watch on that subscriber,
keyed by the erased reference a removal compares, so the camera has nothing
to resolve.

A position that arrives in a payload is proven once, at receipt. A native
actor does that with the ctx verb `resolve_live`
(`crates/aether-substrate/src/actor/native/ctx/address.rs`) and keeps the
proof, not the position. An address that arrives as an `ErasedActorPath`, such as
the component path in a drop or replace request, is proven the same way
through `resolve_path`, whose refusal names the path, never a position. A
guest has the same verb, `WasmCtx::resolve_path`, for a path with no
compile-time actor claim, kept as an `ErasedActorRef`. A path the holder will
send to arrives as a typed path instead
([R-0040](../contributing/design-rules.md#r-0040)): the environment bootstrap
script in `crates/aether-bloomery-bootstrap` now resolves its typed
`ActorPath<JournalActor>` and `ActorPath<BundleDriver>` config fields with
`WasmCtx::resolve` and keeps the two kind-checked `ActorRef`s (#7205),
replacing its erased `resolve_path` proof, through which nothing can be
sent (#6895). A guest has
no door for a payload-borne position and will not get
one, because no guest API takes a `MailboxId`; a guest is told where to send by
an `ActorPath<R>`, a `ProtocolPath<P>`, or the envelope sender.

A typed path that arrives in mail is proven with `resolve`. A native actor
proves a `ProtocolPath<P>` with `ctx.resolve(&path)`
(`crates/aether-substrate/src/actor/native/ctx/address.rs`), which checks that
a `Live` route stands under the path's canonical name and returns a
`ProtocolRef<P>` that sends only the kinds `P` lists (ADR-0231 §3). A guest
proves an `ActorPath<R>` (ADR-0230 §2) the same way, with `WasmCtx::resolve`
(#7205, ADR-0240 D8), the Bloomery bootstrap's own door onto its two peers.
The native `ActorPath<R>` arm still waits for a caller.

## What not to write

- A string literal that repeats the other actor's `NAMESPACE` beside the send.
  `cargo xtask namespaces` reports a literal that repeats another crate's
  declared namespace.
- A hand-folded `MailboxId`.
- `depends` in both directions: both loads refuse.
- A normal dependency in both directions: cargo refuses the cycle.
- A payload field carrying the sender's position, re-resolved at every send.
  The kind names what the sender stands for (`ChannelAttach` names the
  channel); the envelope carries who sent it.
- A stored `ErasedActorRef` meant to be sent through later: no send verb
  takes one. Store a typed proof, an `ActorRef<R>` or a `ProtocolRef<P>`
  ([R-0044](../contributing/design-rules.md#r-0044)).
- An `ErasedActorPath` field its receiver will send to. Carry an
  `ActorPath<R>` or a `ProtocolPath<P>`
  ([R-0040](../contributing/design-rules.md#r-0040)).

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
- [ADR-0231](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0231-protocol-typed-references-and-reply-checks.md)
  — protocol references, the sender cast, and typed paths.
